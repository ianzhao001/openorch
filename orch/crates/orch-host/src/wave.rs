//! Parallel 预设执行接线（M4+）：波次调度器。
//!
//! planner 预置占位（同 preset.rs/B49 先例）：`lib.rs` 声明 `pub mod wave;` 先行入库，
//! 由执行者在本文件内实现契约 API，勿动 lib.rs（frozenPaths）。
//!
//! 规划层（纯函数，r34/slice1+2）：
//!   - `plan_wave_schedule(tasks) -> Vec<WaveStep>` —— 复用 `preset::plan_waves` 分波，每波派生
//!     确定性 `merge_order`（波内并发派发+收取，收取后按 merge_order 串行 verify+merge）。
//!   - `gate_wave(merge_order, outcomes) -> WaveGate` —— 给定一波各任务收取结局，返回可 merge 子集
//!     + 是否干净 + blockers（交主控注入 TaskFailed/AgentDown）。
//! 编排层（IO/并发，r35/slice3）：`run_wave(...)`。

/// Root-manual waves distinguish "waiting for the root fixed-HEAD verdict"
/// from both success and a wave blocker.  Blockers retain precedence.
pub fn root_manual_wave_exit_code(blocked: bool, awaiting_root: usize) -> u8 {
    if blocked {
        1
    } else if awaiting_root > 0 {
        7
    } else {
        0
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct WaveStep {
    pub tasks: Vec<String>,
    pub merge_order: Vec<String>,
}

/// 将任务按 writeSet 规划成有序波次执行计划。
/// 复用 `preset::plan_waves` 分波（writeSet 互斥、保输入序），每波 merge_order 初始等于 tasks 输入序。
pub fn plan_wave_schedule(tasks: &[(String, Vec<String>)]) -> Vec<WaveStep> {
    let waves = crate::preset::plan_waves(tasks);
    waves
        .into_iter()
        .map(|w| WaveStep {
            merge_order: w.clone(),
            tasks: w,
        })
        .collect()
}

/// B147：显式 dependsOn 感知的波次规划。在 [`plan_wave_schedule`] 的
/// writeSet 互斥之上，显式前驱额外强制后继落在**严格更晚**的波次——
/// 显式依赖未 Recorded 的任务绝不可与前驱同波混派（波次串行 + 波闸
/// 保证前驱 Recorded 后后继波才启动）。输入须为拓扑序（ROUND-IR tasks
/// 即 B141 拓扑序）；依赖图上悬空的前驱按已满足计（同 preset 语义，
/// 全图校验在 plan 阶段 fail-closed）。
pub fn plan_wave_schedule_with_dependencies(
    tasks: &[(String, Vec<String>)],
    dependencies: &std::collections::BTreeMap<String, Vec<String>>,
) -> Vec<WaveStep> {
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut placed: Vec<(usize, &str, &[String])> = Vec::new();
    for (task_id, write_set) in tasks {
        let conflict_wave = placed
            .iter()
            .filter(|(_, _, prior_write_set)| {
                crate::preset::write_sets_overlap_glob(write_set, prior_write_set)
            })
            .map(|(prior_wave, _, _)| prior_wave + 1)
            .max()
            .unwrap_or(0);
        let dependency_wave = dependencies
            .get(task_id)
            .into_iter()
            .flat_map(|deps| deps.iter())
            .filter_map(|dependency| {
                placed
                    .iter()
                    .find(|(_, placed_id, _)| placed_id == dependency)
                    .map(|(prior_wave, _, _)| prior_wave + 1)
            })
            .max()
            .unwrap_or(0);
        let wave_index = conflict_wave.max(dependency_wave);
        if wave_index == waves.len() {
            waves.push(Vec::new());
        }
        waves[wave_index].push(task_id.clone());
        placed.push((wave_index, task_id.as_str(), write_set));
    }
    waves
        .into_iter()
        .map(|w| WaveStep {
            merge_order: w.clone(),
            tasks: w,
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum TaskWaveOutcome {
    Collected,
    Failed,
    DeadOrStalled,
}

/// 在每个原始 write-set 波次内按 agent 再拆分，确保同一 agent 在任一子波最多出现一次。
pub fn split_wave_by_agent(
    schedule: &[WaveStep],
    agents: &std::collections::HashMap<String, String>,
) -> Result<Vec<WaveStep>, String> {
    let capacities = agents
        .values()
        .map(|agent| (agent.clone(), 1usize))
        .collect::<std::collections::HashMap<_, _>>();
    split_wave_by_capacity(schedule, agents, &capacities)
}

/// 在每个原始 write-set 波次内按 agent 容量拆分。任务按输入顺序放入首个仍有
/// agent 槽位的子波，原波边界与各子波 merge 顺序均保持不变。
pub fn split_wave_by_capacity(
    schedule: &[WaveStep],
    agents: &std::collections::HashMap<String, String>,
    capacities: &std::collections::HashMap<String, usize>,
) -> Result<Vec<WaveStep>, String> {
    let agent_domains = capacities
        .keys()
        .map(|agent| (agent.clone(), agent.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    split_wave_by_capacity_with_domains(schedule, agents, capacities, &agent_domains, capacities)
}

/// Split each write-set wave against the same immutable agent->domain map used
/// by runtime admission. A task fits a subwave only when both its per-agent
/// count and its shared-domain count remain below their signed limits.
pub fn split_wave_by_capacity_with_domains(
    schedule: &[WaveStep],
    agents: &std::collections::HashMap<String, String>,
    capacities: &std::collections::HashMap<String, usize>,
    agent_domains: &std::collections::HashMap<String, String>,
    domain_capacities: &std::collections::HashMap<String, usize>,
) -> Result<Vec<WaveStep>, String> {
    use std::collections::HashMap;

    let mut split = Vec::new();
    for step in schedule {
        let resolved = step
            .tasks
            .iter()
            .map(|task| {
                let agent = agents
                    .get(task)
                    .map(|agent| agent.trim())
                    .filter(|agent| !agent.is_empty())
                    .ok_or_else(|| format!("任务 {task} 缺少有效 agent"))?;
                let capacity = capacities
                    .get(agent)
                    .copied()
                    .ok_or_else(|| format!("任务 {task} 的 agent {agent} 容量未知"))?;
                if capacity == 0 {
                    return Err(format!("任务 {task} 的 agent {agent} 容量为 0"));
                }
                let domain = agent_domains
                    .get(agent)
                    .map(|domain| domain.trim())
                    .filter(|domain| !domain.is_empty())
                    .ok_or_else(|| format!("任务 {task} 的 agent {agent} 缺少有效 quotaDomain"))?;
                let domain_capacity = domain_capacities
                    .get(domain)
                    .copied()
                    .ok_or_else(|| format!("任务 {task} 的 quotaDomain {domain} 容量未知"))?;
                if domain_capacity == 0 {
                    return Err(format!("任务 {task} 的 quotaDomain {domain} 容量为 0"));
                }
                Ok((
                    task.clone(),
                    agent.to_string(),
                    capacity,
                    domain.to_string(),
                    domain_capacity,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;

        let mut subwaves: Vec<(HashMap<String, usize>, HashMap<String, usize>, Vec<String>)> =
            Vec::new();
        for (task, agent, capacity, domain, domain_capacity) in resolved {
            if let Some((used_agents, used_domains, tasks)) =
                subwaves.iter_mut().find(|(used_agents, used_domains, _)| {
                    used_agents.get(&agent).copied().unwrap_or(0) < capacity
                        && used_domains.get(&domain).copied().unwrap_or(0) < domain_capacity
                })
            {
                *used_agents.entry(agent).or_insert(0) += 1;
                *used_domains.entry(domain).or_insert(0) += 1;
                tasks.push(task);
            } else {
                subwaves.push((
                    HashMap::from([(agent, 1usize)]),
                    HashMap::from([(domain, 1usize)]),
                    vec![task],
                ));
            }
        }

        for (_, _, tasks) in subwaves {
            let merge_order = step
                .merge_order
                .iter()
                .filter(|task| tasks.contains(task))
                .cloned()
                .collect();
            split.push(WaveStep { tasks, merge_order });
        }
    }
    Ok(split)
}

/// run-wave 在任何真实派发前必须满足的只读前置事实。
#[derive(Debug, Clone)]
pub struct WavePrerequisites {
    pub plan_signed_off: bool,
    pub bad_ledger_lines: usize,
    pub ir_digest_matches: bool,
    pub task_ids: Vec<String>,
    pub seeded_red_tasks: Vec<String>,
    pub seed_verified_tasks: std::collections::HashSet<String>,
}

pub fn validate_wave_prerequisites(prerequisites: &WavePrerequisites) -> Result<(), String> {
    if !prerequisites.plan_signed_off {
        return Err("当前轮缺少 PlanSignedOff".into());
    }
    if prerequisites.bad_ledger_lines != 0 {
        return Err(format!(
            "当前轮账本含 {} 个坏行",
            prerequisites.bad_ledger_lines
        ));
    }
    if !prerequisites.ir_digest_matches {
        return Err("当前卡片重编译 IR 或 TaskValidated digest 与 ROUND-IR 不一致".into());
    }
    for task in &prerequisites.seeded_red_tasks {
        if !prerequisites.task_ids.contains(task) {
            return Err(format!("seeded-red 任务 {task} 不在当前 ROUND-IR"));
        }
        if !prerequisites.seed_verified_tasks.contains(task) {
            return Err(format!("seeded-red 任务 {task} 缺少 SeedOracleVerified"));
        }
    }
    Ok(())
}

/// 并发收取任务，结果稳定地按输入顺序返回；单个 worker panic 只降级该任务。
pub fn collect_tasks_with<F>(tasks: &[String], collect: &F) -> Vec<(String, TaskWaveOutcome)>
where
    F: Fn(&str) -> TaskWaveOutcome + Sync + ?Sized,
{
    std::thread::scope(|scope| {
        let handles = tasks
            .iter()
            .map(|task| {
                let task_id = task.clone();
                (task.clone(), scope.spawn(move || collect(task_id.as_str())))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(task, handle)| {
                let outcome = handle.join().unwrap_or(TaskWaveOutcome::Failed);
                (task, outcome)
            })
            .collect()
    })
}

/// 进程级 run-wave single-flight lease。锁文件只是 rendezvous，互斥由 OS advisory lock 保证。
pub struct WaveLease {
    release: Option<std::sync::mpsc::SyncSender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WaveLease {
    pub fn acquire(root: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::{bail, Context};
        use std::fs::OpenOptions;
        use std::sync::mpsc::sync_channel;

        let lock_dir = root.join("coordination/runtime/locks");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("创建 run-wave 锁目录失败: {}", lock_dir.display()))?;
        let lock_path = lock_dir.join("run-wave.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("打开 run-wave 锁文件失败: {}", lock_path.display()))?;

        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        let (release_tx, release_rx) = sync_channel::<()>(0);
        let worker = std::thread::spawn(move || {
            let mut lock = fd_lock::RwLock::new(file);
            match lock.try_write() {
                Ok(_guard) => {
                    let _ = ready_tx.send(Ok(()));
                    let _ = release_rx.recv();
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            };
        });

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                release: Some(release_tx),
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                bail!("已有 run-wave 持有 single-flight lease: {error}");
            }
            Err(error) => {
                let _ = worker.join();
                bail!("run-wave lease worker 启动失败: {error}");
            }
        }
    }
}

impl Drop for WaveLease {
    fn drop(&mut self) {
        self.release.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 波次恢复所依据的任务相位。
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum WaveTaskPhase {
    New,
    Dispatched,
    ReadyForVerification,
    Approved,
    Recorded,
    NeedsOperator,
}

/// 将账本投影相位与运行中动作合成为波次相位。
///
/// 新鲜的 VerifyStarted/MergeStarted 优先于任何投影状态：动作可能已经在另一个
/// 进程中发生，run-wave 不能在无法证明其结束前重复执行。
pub fn classify_wave_phase(
    state: Option<orch_core::TaskState>,
    fresh_inflight: bool,
) -> WaveTaskPhase {
    if fresh_inflight {
        return WaveTaskPhase::NeedsOperator;
    }
    match state {
        None => WaveTaskPhase::New,
        Some(orch_core::TaskState::Dispatched) => WaveTaskPhase::Dispatched,
        Some(orch_core::TaskState::ReadyForVerification) => WaveTaskPhase::ReadyForVerification,
        Some(orch_core::TaskState::Approved) => WaveTaskPhase::Approved,
        Some(orch_core::TaskState::Recorded) => WaveTaskPhase::Recorded,
        Some(
            orch_core::TaskState::ChangesRequested
            | orch_core::TaskState::Blocked
            | orch_core::TaskState::Merged
            | orch_core::TaskState::Reopened,
        ) => WaveTaskPhase::NeedsOperator,
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct WaveGate {
    pub mergeable: Vec<String>,
    pub clean: bool,
    pub blockers: Vec<String>,
}

/// 门控一波中各任务的执行结局，计算可 merge 的任务、波次是否干净以及阻碍任务。
pub fn gate_wave(merge_order: &[String], outcomes: &[(String, TaskWaveOutcome)]) -> WaveGate {
    let mut mergeable = Vec::new();
    let mut blockers = Vec::new();

    for id in merge_order {
        let outcome = outcomes
            .iter()
            .find(|(task_id, _)| task_id == id)
            .map(|(_, o)| o);
        if outcome == Some(&TaskWaveOutcome::Collected) {
            mergeable.push(id.clone());
        } else {
            blockers.push(id.clone());
        }
    }

    let clean = blockers.is_empty();

    WaveGate {
        mergeable,
        clean,
        blockers,
    }
}

// ── 编排层（r35/slice3）──

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum WaveBlockerClass {
    NeedsOperator,
    DispatchFailed,
    CollectFailed,
    DeadOrStalled,
    VerifyFailed,
    MergeFailed,
}

impl WaveBlockerClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsOperator => "NeedsOperator",
            Self::DispatchFailed => "DispatchFailed",
            Self::CollectFailed => "CollectFailed",
            Self::DeadOrStalled => "DeadOrStalled",
            Self::VerifyFailed => "VerifyFailed",
            Self::MergeFailed => "MergeFailed",
        }
    }
}

impl std::fmt::Display for WaveBlockerClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct TaskWaveBlocker {
    pub task_id: String,
    pub class: WaveBlockerClass,
}

fn blocker_inbox_body(round: &str, blocker: &TaskWaveBlocker, attempt_key: &str) -> String {
    format!(
        "---\npriority: 9\nround-hint: {round}\ntask-id: {}\nblocker-class: {}\nattempt-key: {attempt_key}\n---\n# Wave task blocker\n\nTask: `{}`\n\nClass: `{}`\n\nAttempt: `{attempt_key}`\n\nResolve this task-level blocker before continuing later waves.\n",
        blocker.task_id, blocker.class, blocker.task_id, blocker.class
    )
}

fn ensure_blocker_inbox(
    root: &std::path::Path,
    relative_path: &str,
    expected: &[u8],
) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use std::io::Write;

    let path = root.join(relative_path);
    match std::fs::read(&path) {
        Ok(existing) => {
            if existing != expected {
                bail!(
                    "blocker inbox 已存在但内容不一致，拒绝覆盖: {}",
                    path.display()
                );
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 blocker inbox 失败: {}", path.display()));
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 blocker inbox 目录失败: {}", parent.display()))?;
    }
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(expected)
                .with_context(|| format!("写 blocker inbox 失败: {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("同步 blocker inbox 失败: {}", path.display()))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read(&path)
                .with_context(|| format!("并发重读 blocker inbox 失败: {}", path.display()))?;
            if existing != expected {
                bail!(
                    "blocker inbox 并发创建但内容不一致，拒绝覆盖: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("创建 blocker inbox 失败: {}", path.display()))
        }
    }
}

/// 以 round/task/class/attempt 为幂等键记录任务级波次 blocker。
///
/// inbox 先于事件落盘，使事件追加崩溃时仍有 planner 可见指令；重试会核对已有文件内容，
/// 并在同一 ledger.lock 内重新读取账本、判重、追加 EscalationRaised。
pub fn record_wave_blocker(
    root: &std::path::Path,
    round: &str,
    blocker: &TaskWaveBlocker,
) -> anyhow::Result<String> {
    use anyhow::{bail, Context};
    use sha2::{Digest, Sha256};

    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let initial = orch_core::read_ledger(&ledger_path).with_context(|| {
        format!(
            "记录 wave blocker 前读取账本失败: {}",
            ledger_path.display()
        )
    })?;
    if !initial.bad_lines.is_empty() {
        bail!(
            "记录 wave blocker 前发现账本坏行：{} 行（首处第 {} 行）",
            initial.bad_lines.len(),
            initial.bad_lines[0].0
        );
    }

    let mut recorded_path = None;
    crate::ledger::append_checked(root, round, |events| {
        let attempt_key = events
            .iter()
            .rev()
            .find(|event| {
                event.kind == "DispatchIssued"
                    && event.task_id.as_deref() == Some(blocker.task_id.as_str())
                    && event.round.as_deref() == Some(round)
            })
            .map(|event| event.event_id.as_str())
            .unwrap_or("pre-dispatch");
        let class = blocker.class.as_str();
        let identity = format!(
            "{round}\u{0}{}\u{0}{class}\u{0}{attempt_key}",
            blocker.task_id
        );
        let digest = hex::encode(Sha256::digest(identity.as_bytes()));
        let inbox_path = format!("coordination/inbox/wave-task-blocker-{}.md", &digest[..20]);
        let reason = format!(
            "run-wave task {} blocked with class {class}",
            blocker.task_id
        );
        let hint = format!("inspect {inbox_path} and resolve before continuing later waves");
        let body = blocker_inbox_body(round, blocker, attempt_key);

        let matching = events
            .iter()
            .filter(|event| {
                let payload = event.payload.as_ref();
                event.kind == "EscalationRaised"
                    && event.task_id.as_deref() == Some(blocker.task_id.as_str())
                    && event.round.as_deref() == Some(round)
                    && payload.and_then(|value| value.get("stage"))
                        == Some(&serde_json::json!("wave-task-blocker"))
                    && payload.and_then(|value| value.get("blockerClass"))
                        == Some(&serde_json::json!(class))
                    && payload.and_then(|value| value.get("attemptKey"))
                        == Some(&serde_json::json!(attempt_key))
            })
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            bail!(
                "wave blocker 幂等键已有 {} 条事件，拒绝继续",
                matching.len()
            );
        }

        if let Some(existing) = matching.first() {
            let payload = existing
                .payload
                .as_ref()
                .context("wave blocker 事件缺 payload")?;
            for (field, expected) in [
                ("reason", reason.as_str()),
                ("hint", hint.as_str()),
                ("inboxPath", inbox_path.as_str()),
            ] {
                if payload.get(field).and_then(serde_json::Value::as_str) != Some(expected) {
                    bail!("wave blocker 既有事件字段 {field} 不一致，拒绝继续");
                }
            }
            ensure_blocker_inbox(root, &inbox_path, body.as_bytes())?;
            recorded_path = Some(inbox_path);
            return Ok(Vec::new());
        }

        ensure_blocker_inbox(root, &inbox_path, body.as_bytes())?;
        let event = crate::ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some(&blocker.task_id),
            Some(round),
            serde_json::json!({
                "stage": "wave-task-blocker",
                "blockerClass": class,
                "attemptKey": attempt_key,
                "reason": reason,
                "hint": hint,
                "inboxPath": inbox_path,
            }),
        );
        recorded_path = event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("inboxPath"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        Ok(vec![event])
    })?;

    recorded_path.context("记录 wave blocker 后未得到 inbox 路径")
}

/// 可注入的波次驱动 seam：用 fake 实现可无真 spawn 测试编排逻辑。
pub trait WaveDriver: Sync {
    /// 当前任务相位。默认实现保留旧 fake 的 already_recorded 兼容性，其余任务按 New 处理。
    fn phase(&self, task: &str) -> WaveTaskPhase {
        if self.already_recorded(task) {
            WaveTaskPhase::Recorded
        } else {
            WaveTaskPhase::New
        }
    }
    /// 幂等：已 recorded 的任务跳过（崩溃重跑跳过已收）。
    fn already_recorded(&self, task: &str) -> bool;
    /// 串行派发（模型唤醒预算检查点）；true=派发成功。
    fn dispatch(&self, task: &str) -> bool;
    /// 驱到收取 → 映射结局（波内并发）。
    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome;
    /// 串行验证；true=验证通过。
    fn verify(&self, task: &str) -> bool;
    /// 串行合并；true=已合并。
    fn merge(&self, task: &str) -> bool;
    /// Root-manual Approved is only a historical projection until the exact
    /// current root verdict/refs/artifact tuple revalidates.  Safe default is
    /// false so a new driver cannot accidentally turn an actor-agnostic PASS
    /// into merge authority.
    fn root_merge_authorized(&self, _task: &str) -> bool {
        false
    }
    /// 任务级 blocker 持久化 hook。默认 no-op 保持旧 fake/source compatibility。
    fn record_blocker(&self, _blocker: &TaskWaveBlocker) -> bool {
        true
    }
}

/// 波次调度结果。
#[derive(Debug, PartialEq)]
pub struct WaveRunOutcome {
    pub merged: Vec<String>,
    pub blocked_wave: Option<usize>,
}

#[derive(Debug, PartialEq)]
pub struct RootManualWaveOutcome {
    pub wave: WaveRunOutcome,
    pub awaiting_root: Vec<String>,
}

/// CLI 退出码：全波干净为 0，任一波阻断为 1。
pub fn wave_exit_code(outcome: &WaveRunOutcome) -> u8 {
    if outcome.blocked_wave.is_some() {
        1
    } else {
        0
    }
}

/// 纯编排逻辑（种子直测）：任务失败隔离在本波，merge 失败则越过不可逆边界立即停。
pub fn run_wave_with(schedule: &[WaveStep], driver: &dyn WaveDriver) -> WaveRunOutcome {
    run_wave_with_mode(schedule, driver, false).wave
}

pub fn run_wave_with_mode(
    schedule: &[WaveStep],
    driver: &dyn WaveDriver,
    root_manual: bool,
) -> RootManualWaveOutcome {
    let mut merged = Vec::new();
    let mut blocked_wave = None;
    let mut awaiting_root = Vec::new();

    for (wave_idx, step) in schedule.iter().enumerate() {
        // ① 先读取每个任务的相位。相位决定恢复起点，Recorded 完全跳过，
        // NeedsOperator 只阻断本波，不妨碍本波其他安全任务收口。
        let phases: Vec<(String, WaveTaskPhase)> = step
            .tasks
            .iter()
            .map(|task| (task.clone(), driver.phase(task)))
            .collect();
        let pending: Vec<&str> = phases
            .iter()
            .filter(|(_, phase)| *phase != WaveTaskPhase::Recorded)
            .map(|(task, _)| task.as_str())
            .collect();
        let mut wave_blocked = false;
        let mut blocker_tasks = std::collections::HashSet::new();
        for (task, phase) in &phases {
            if *phase == WaveTaskPhase::NeedsOperator {
                let blocker = TaskWaveBlocker {
                    task_id: task.clone(),
                    class: WaveBlockerClass::NeedsOperator,
                };
                driver.record_blocker(&blocker);
                blocker_tasks.insert(task.clone());
                wave_blocked = true;
            }
        }

        // ② New 任务串行 dispatch；Dispatched 任务直接进入 collect。
        // dispatch 失败只隔离该 task，同波 clean peer 仍继续。
        let mut to_collect = Vec::new();
        let mut outcomes = Vec::new();
        for (task, phase) in &phases {
            match phase {
                WaveTaskPhase::New => {
                    if driver.dispatch(task) {
                        to_collect.push(task.as_str());
                    } else {
                        let blocker = TaskWaveBlocker {
                            task_id: task.clone(),
                            class: WaveBlockerClass::DispatchFailed,
                        };
                        driver.record_blocker(&blocker);
                        blocker_tasks.insert(task.clone());
                        wave_blocked = true;
                    }
                }
                WaveTaskPhase::Dispatched => to_collect.push(task.as_str()),
                WaveTaskPhase::ReadyForVerification | WaveTaskPhase::Approved => {
                    // 这些相位已经完成 collect；用 Collected 让它们进入 gate。
                    outcomes.push((task.clone(), TaskWaveOutcome::Collected));
                }
                WaveTaskPhase::Recorded | WaveTaskPhase::NeedsOperator => {}
            }
        }

        // ③ 仅对需要收取的任务并发 drive_to_collect，按输入序归集结局。
        let collect_tasks = to_collect
            .iter()
            .map(|task| (*task).to_string())
            .collect::<Vec<_>>();
        let collected = collect_tasks_with(&collect_tasks, &|task| driver.drive_to_collect(task));
        for (task, outcome) in &collected {
            let class = match outcome {
                TaskWaveOutcome::Collected => None,
                TaskWaveOutcome::Failed => Some(WaveBlockerClass::CollectFailed),
                TaskWaveOutcome::DeadOrStalled => Some(WaveBlockerClass::DeadOrStalled),
            };
            if let Some(class) = class {
                let blocker = TaskWaveBlocker {
                    task_id: task.clone(),
                    class,
                };
                driver.record_blocker(&blocker);
                blocker_tasks.insert(task.clone());
                wave_blocked = true;
            }
        }
        outcomes.extend(collected);

        // ④ merge_order 只保留本波非 Recorded 任务；没有成功 outcome 的任务天然阻断。
        let merge_order: Vec<String> = step
            .merge_order
            .iter()
            .filter(|task| pending.contains(&task.as_str()))
            .cloned()
            .collect();
        let gate = gate_wave(&merge_order, &outcomes);
        for task in &gate.blockers {
            if blocker_tasks.insert(task.clone()) {
                let blocker = TaskWaveBlocker {
                    task_id: task.clone(),
                    class: WaveBlockerClass::CollectFailed,
                };
                driver.record_blocker(&blocker);
                wave_blocked = true;
            }
        }

        // ⑤ verify 失败只隔离该 task；merge 失败越过不可逆边界，立即停止后续动作。
        for task in &gate.mergeable {
            let phase = phases
                .iter()
                .find(|(candidate, _)| candidate == task)
                .map(|(_, phase)| phase);
            if root_manual
                && (phase != Some(&WaveTaskPhase::Approved) || !driver.root_merge_authorized(task))
            {
                awaiting_root.push(task.clone());
                continue;
            }
            if phase != Some(&WaveTaskPhase::Approved) && !driver.verify(task) {
                driver.record_blocker(&TaskWaveBlocker {
                    task_id: task.clone(),
                    class: WaveBlockerClass::VerifyFailed,
                });
                wave_blocked = true;
                continue;
            }
            if !driver.merge(task) {
                driver.record_blocker(&TaskWaveBlocker {
                    task_id: task.clone(),
                    class: WaveBlockerClass::MergeFailed,
                });
                wave_blocked = true;
                break;
            }
            merged.push(task.clone());
        }

        // ⑥ 本波任一 blocker 都阻断后续波；merge 失败已在本波内立即停止。
        if wave_blocked {
            blocked_wave = Some(wave_idx);
            break;
        }
        if !awaiting_root.is_empty() {
            break;
        }
    }

    RootManualWaveOutcome {
        wave: WaveRunOutcome {
            merged,
            blocked_wave,
        },
        awaiting_root,
    }
}

/// 真实 IO 版波次调度器：枚举当前轮任务 → plan_wave_schedule → 生产 WaveDriver → run_wave_with。
pub fn run_wave(
    root: &std::path::Path,
    timeout_secs: u64,
) -> anyhow::Result<RootManualWaveOutcome> {
    use crate::current_round;
    use anyhow::Context;
    use std::collections::{HashMap, HashSet};

    let _lease = WaveLease::acquire(root)?;
    let round = current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取当前轮账本失败: {}", ledger_path.display()))?;
    let ir = crate::plan::require_active_round_ir(root, &round, &ledger.events)?;
    let task_ids = ir
        .candidate
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    let seeded_red_tasks = ir
        .candidate
        .tasks
        .iter()
        .filter(|task| task.seed_protocol == "seeded-red")
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    let seed_verified_tasks = ledger
        .events
        .iter()
        .filter(|event| {
            event.kind == "SeedOracleVerified" && event.round.as_deref() == Some(&round)
        })
        .filter_map(|event| event.task_id.clone())
        .collect::<HashSet<_>>();
    let prerequisites = WavePrerequisites {
        plan_signed_off: true,
        bad_ledger_lines: ledger.bad_lines.len(),
        ir_digest_matches: ir.digest_matches,
        task_ids,
        seeded_red_tasks,
        seed_verified_tasks,
    };
    validate_wave_prerequisites(&prerequisites).map_err(anyhow::Error::msg)?;

    let task_list = ir
        .candidate
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.write_set.clone()))
        .collect::<Vec<_>>();
    let agents = ir
        .candidate
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.agent.clone()))
        .collect::<HashMap<_, _>>();
    let task_agents = agents.values().cloned().collect::<Vec<_>>();
    let mut capacities = HashMap::new();
    let mut agent_domains = HashMap::new();
    let mut domain_capacities = HashMap::new();
    for agent in task_agents {
        let scheduling = &ir.candidate.scheduling;
        let cap = scheduling
            .effective_agent_capacity(&agent)
            .map_err(anyhow::Error::msg)?;
        let domain = scheduling
            .quota_domain_for(&agent)
            .map_err(anyhow::Error::msg)?
            .to_string();
        let domain_capacity = scheduling
            .effective_domain_capacity_for_agent(&agent)
            .map_err(anyhow::Error::msg)?;
        if let Some(previous) = domain_capacities.insert(domain.clone(), domain_capacity) {
            if previous != domain_capacity {
                anyhow::bail!(
                    "ROUND-IR quotaDomain {domain} 对不同 agent 给出冲突容量: {previous} vs {domain_capacity}"
                );
            }
        }
        capacities.insert(agent.clone(), cap);
        agent_domains.insert(agent, domain);
    }
    // B147：可派发判定 = 显式 dependsOn 全 Recorded（严格更晚波次）∧
    // writeSet 冲突前驱全 Recorded（writeSet 分波）∧ capacity 可用（容量再分波）。
    let dependencies = ir
        .candidate
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.depends_on.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let schedule = split_wave_by_capacity_with_domains(
        &plan_wave_schedule_with_dependencies(&task_list, &dependencies),
        &agents,
        &capacities,
        &agent_domains,
        &domain_capacities,
    )
    .map_err(anyhow::Error::msg)?;

    let driver = RealDriver {
        root,
        timeout_secs,
        model: ir.candidate.verification.model.clone(),
        liveness: ir.candidate.liveness.clone(),
        root_manual: ir.candidate.verification.mode == "root-manual-fixed-head",
        ir_revision: ir.persisted_revision,
        validation_digest: ir.persisted_digest.clone(),
    };
    let out = run_wave_with_mode(
        &schedule,
        &driver,
        ir.candidate.verification.mode == "root-manual-fixed-head",
    );
    Ok(out)
}

#[cfg(test)]
mod b90_source_guards {
    #[test]
    fn real_run_wave_wires_lease_prerequisites_and_agent_split_before_driver() {
        let source = include_str!("wave.rs");
        let body = source
            .split_once("pub fn run_wave(")
            .unwrap()
            .1
            .split_once("struct WaveLedgerSnapshot")
            .unwrap()
            .0;
        let driver = body.find("let driver = RealDriver").unwrap();
        for required in [
            "WaveLease::acquire",
            "require_active_round_ir",
            "validate_wave_prerequisites",
            "split_wave_by_capacity",
        ] {
            assert!(
                body.find(required)
                    .is_some_and(|position| position < driver),
                "real run_wave must invoke {required} before constructing its driver"
            );
        }
    }

    #[test]
    fn real_wave_collection_uses_panic_safe_helper() {
        let source = include_str!("wave.rs");
        let body = source
            .split_once("pub fn run_wave_with(")
            .unwrap()
            .1
            .split_once("/// 真实 IO 版波次调度器")
            .unwrap()
            .0;
        assert!(body.contains("collect_tasks_with"));
    }
}

struct WaveLedgerSnapshot {
    states: std::collections::HashMap<String, orch_core::TaskState>,
    fresh_inflight: std::collections::HashSet<String>,
    bad_lines: Vec<(usize, String)>,
}

struct RealDriver<'a> {
    root: &'a std::path::Path,
    timeout_secs: u64,
    model: Option<String>,
    liveness: crate::plan::IrLiveness,
    root_manual: bool,
    ir_revision: u32,
    validation_digest: String,
}

impl<'a> RealDriver<'a> {
    fn active_contract_is_current(&self) -> bool {
        let round = match crate::current_round(self.root) {
            Ok(round) => round,
            Err(_) => return false,
        };
        let path = self
            .root
            .join(format!("coordination/rounds/{round}/events.jsonl"));
        match orch_core::read_ledger(&path) {
            Ok(ledger) if ledger.bad_lines.is_empty() => {
                crate::plan::require_active_round_ir(self.root, &round, &ledger.events).is_ok_and(
                    |active| {
                        active.persisted_revision == self.ir_revision
                            && active.persisted_digest == self.validation_digest
                    },
                )
            }
            _ => false,
        }
    }

    /// 每个 phase/action 都取得当前轮账本快照，不复用 run_wave 启动时的投影。
    fn ledger_snapshot(&self) -> anyhow::Result<WaveLedgerSnapshot> {
        use orch_core::{fold, read_ledger};
        use std::time::{SystemTime, UNIX_EPOCH};

        let round = crate::current_round(self.root)?;
        let ledger_path = self
            .root
            .join(format!("coordination/rounds/{round}/events.jsonl"));
        let lr = read_ledger(&ledger_path)?;
        let projection = fold(&lr.events);
        let now_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let timed_events = lr
            .events
            .iter()
            .filter_map(|event| {
                event.task_id.clone().map(|task| {
                    let ts = humantime::parse_rfc3339(&event.ts)
                        .ok()
                        .and_then(|parsed| parsed.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs() as i64)
                        .unwrap_or(now_epoch);
                    (task, event.kind.clone(), ts)
                })
            })
            .collect::<Vec<_>>();
        let timeout_ttl = i64::try_from(self.timeout_secs).unwrap_or(i64::MAX);
        let ttl_secs = crate::runloop::INFLIGHT_TTL_SECS.max(timeout_ttl);
        let (fresh, _) =
            crate::runloop::infer_inflight_with_ttl(&timed_events, now_epoch, ttl_secs);

        Ok(WaveLedgerSnapshot {
            states: projection
                .tasks
                .into_iter()
                .filter_map(|(task, projection)| projection.state.map(|state| (task, state)))
                .collect(),
            fresh_inflight: fresh.into_iter().collect(),
            bad_lines: lr.bad_lines,
        })
    }

    fn classify_snapshot(snapshot: &WaveLedgerSnapshot, task: &str) -> WaveTaskPhase {
        classify_wave_phase(
            snapshot.states.get(task).copied(),
            snapshot.fresh_inflight.contains(task),
        )
    }

    fn report_bad_ledger(&self, action: &str, bad_lines: &[(usize, String)]) {
        eprintln!(
            "⚠️ {action} 拒绝在坏账本上执行：{} 坏行（首个 #{}）",
            bad_lines.len(),
            bad_lines.first().map(|(line, _)| *line).unwrap_or(0)
        );
    }
}

impl<'a> WaveDriver for RealDriver<'a> {
    fn phase(&self, task: &str) -> WaveTaskPhase {
        match self.ledger_snapshot() {
            Ok(snapshot) if snapshot.bad_lines.is_empty() => {
                Self::classify_snapshot(&snapshot, task)
            }
            Ok(snapshot) => {
                self.report_bad_ledger("phase", &snapshot.bad_lines);
                WaveTaskPhase::NeedsOperator
            }
            Err(error) => {
                eprintln!("⚠️ phase {task} 重读账本失败: {error:#}");
                WaveTaskPhase::NeedsOperator
            }
        }
    }

    fn already_recorded(&self, task: &str) -> bool {
        match self.ledger_snapshot() {
            Ok(snapshot) if snapshot.bad_lines.is_empty() => {
                Self::classify_snapshot(&snapshot, task) == WaveTaskPhase::Recorded
            }
            _ => false,
        }
    }

    fn dispatch(&self, task: &str) -> bool {
        if !self.active_contract_is_current() {
            eprintln!("⚠️ dispatch {task} 拒绝：active ROUND-IR/signoff 已漂移");
            return false;
        }
        // 动作前重读账本；坏行或新鲜 in-flight 一律 fail-closed。
        let before = match self.ledger_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("⚠️ dispatch {task} 前重读账本失败: {error:#}");
                return false;
            }
        };
        if !before.bad_lines.is_empty() {
            self.report_bad_ledger("dispatch", &before.bad_lines);
            return false;
        }
        let phase = Self::classify_snapshot(&before, task);
        if phase != WaveTaskPhase::New {
            // 另一执行者可能已经把 DispatchIssued 写入账本；此时继续 collect，
            // 但绝不再次调用 run_dispatch。
            return phase == WaveTaskPhase::Dispatched;
        }

        if !self.active_contract_is_current() {
            eprintln!("⚠️ dispatch {task} 临界前拒绝：active ROUND-IR/signoff 已漂移");
            return false;
        }
        match crate::tierf::run_dispatch(self.root, task, false) {
            Ok(_) => true,
            Err(error) => {
                eprintln!("⚠️ dispatch {task} 失败: {error:#}");
                // run_dispatch 可能已持久化 DispatchIssued 后才在唤醒/收尾阶段报错。
                // 只有重读确认该事实且仍为 Dispatched，才允许本波续跑。
                match self.ledger_snapshot() {
                    Ok(after) if after.bad_lines.is_empty() => {
                        Self::classify_snapshot(&after, task) == WaveTaskPhase::Dispatched
                    }
                    Ok(after) => {
                        self.report_bad_ledger("dispatch 失败后的复核", &after.bad_lines);
                        false
                    }
                    Err(read_error) => {
                        eprintln!("⚠️ dispatch {task} 失败后的账本复核失败: {read_error:#}");
                        false
                    }
                }
            }
        }
    }

    fn drive_to_collect(&self, task: &str) -> TaskWaveOutcome {
        use crate::tierf::AwaitOutcome;

        if !self.active_contract_is_current() {
            eprintln!("⚠️ collect {task} 拒绝：active ROUND-IR/signoff 已漂移");
            return TaskWaveOutcome::DeadOrStalled;
        }
        let snapshot = match self.ledger_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("⚠️ collect {task} 前重读账本失败: {error:#}");
                return TaskWaveOutcome::DeadOrStalled;
            }
        };
        if !snapshot.bad_lines.is_empty() {
            self.report_bad_ledger("collect", &snapshot.bad_lines);
            return TaskWaveOutcome::DeadOrStalled;
        }
        if Self::classify_snapshot(&snapshot, task) != WaveTaskPhase::Dispatched {
            eprintln!("⚠️ collect {task} 拒绝：当前相位不是 Dispatched");
            return TaskWaveOutcome::DeadOrStalled;
        }

        let defaults = crate::liveness::LivenessOpts::default();
        let live = Some(crate::liveness::LivenessOpts {
            stall: std::time::Duration::from_secs(
                self.liveness.working_stall_minutes.saturating_mul(60),
            ),
            probe_every: std::time::Duration::from_secs(self.liveness.monitor_seconds),
            confirm: u8::try_from(self.liveness.confirm_samples).unwrap_or(u8::MAX),
            ..defaults
        });
        if !self.active_contract_is_current() {
            eprintln!("⚠️ collect {task} 临界前拒绝：active ROUND-IR/signoff 已漂移");
            return TaskWaveOutcome::DeadOrStalled;
        }
        let outcome = match crate::tierf::run_await(self.root, task, self.timeout_secs, live) {
            Ok(AwaitOutcome::Collected(_)) => TaskWaveOutcome::Collected,
            Ok(AwaitOutcome::Blocked { .. }) => TaskWaveOutcome::DeadOrStalled,
            Ok(AwaitOutcome::LivenessDead { .. }) => TaskWaveOutcome::DeadOrStalled,
            Ok(AwaitOutcome::LivenessStalled { .. }) => TaskWaveOutcome::DeadOrStalled,
            Err(error) => {
                eprintln!("⚠️ await {task} 失败: {error:#}");
                TaskWaveOutcome::Failed
            }
        };
        if !self.active_contract_is_current() {
            eprintln!(
                "⚠️ collect {task} 完成后发现 active ROUND-IR/signoff 漂移；旧 collect 不得续用"
            );
            return TaskWaveOutcome::DeadOrStalled;
        }
        outcome
    }

    fn verify(&self, task: &str) -> bool {
        let Some(model) = self.model.as_deref() else {
            eprintln!("⚠️ legacy verify {task} 缺 mode verifier model");
            return false;
        };
        let snapshot = match self.ledger_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("⚠️ verify {task} 前重读账本失败: {error:#}");
                return false;
            }
        };
        if !snapshot.bad_lines.is_empty() {
            self.report_bad_ledger("verify", &snapshot.bad_lines);
            return false;
        }
        if Self::classify_snapshot(&snapshot, task) != WaveTaskPhase::ReadyForVerification {
            eprintln!("⚠️ verify {task} 拒绝：当前相位不是 ReadyForVerification");
            return false;
        }

        let round = match crate::current_round(self.root) {
            Ok(round) => round,
            Err(error) => {
                eprintln!("⚠️ verify {task} 获取当前轮失败: {error:#}");
                return false;
            }
        };
        if let Err(error) = crate::ledger::append(
            self.root,
            &round,
            &[crate::ledger::event(
                "VerifyStarted",
                "runtime:orch",
                Some(task),
                Some(&round),
                serde_json::json!({
                    "model": model,
                    "timeoutSecs": self.timeout_secs,
                }),
            )],
        ) {
            eprintln!("⚠️ verify {task} 写 VerifyStarted 失败: {error:#}");
            return false;
        }

        match crate::verify::run_verify(self.root, task, model, self.timeout_secs) {
            Ok(outcome) => {
                outcome.verdict.eq_ignore_ascii_case("PASS")
                    || outcome.verdict.eq_ignore_ascii_case("Approved")
            }
            Err(error) => {
                eprintln!("⚠️ verify {task} 失败: {error:#}");
                false
            }
        }
    }

    fn merge(&self, task: &str) -> bool {
        if !self.active_contract_is_current() {
            eprintln!("⚠️ merge {task} 拒绝：active ROUND-IR/signoff 已漂移");
            return false;
        }
        let snapshot = match self.ledger_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!("⚠️ merge {task} 前重读账本失败: {error:#}");
                return false;
            }
        };
        if !snapshot.bad_lines.is_empty() {
            self.report_bad_ledger("merge", &snapshot.bad_lines);
            return false;
        }
        if Self::classify_snapshot(&snapshot, task) != WaveTaskPhase::Approved {
            eprintln!("⚠️ merge {task} 拒绝：当前相位不是 Approved");
            return false;
        }

        if !self.active_contract_is_current() {
            eprintln!("⚠️ merge {task} 临界前拒绝：active ROUND-IR/signoff 已漂移");
            return false;
        }
        match crate::close::run_merge(self.root, task) {
            Ok(_) => true,
            Err(error) => {
                eprintln!("⚠️ merge {task} 失败: {error:#}");
                false
            }
        }
    }

    fn root_merge_authorized(&self, task: &str) -> bool {
        if !self.active_contract_is_current() {
            return false;
        }
        if !self.root_manual {
            return true;
        }
        let round = match crate::current_round(self.root) {
            Ok(round) => round,
            Err(_) => return false,
        };
        let ledger_path = self
            .root
            .join(format!("coordination/rounds/{round}/events.jsonl"));
        let ledger = match orch_core::read_ledger(&ledger_path) {
            Ok(ledger) if ledger.bad_lines.is_empty() => ledger,
            _ => return false,
        };
        crate::verify::validate_root_merge_authorization(self.root, &round, task, &ledger.events)
            .is_ok()
    }

    fn record_blocker(&self, blocker: &TaskWaveBlocker) -> bool {
        let round = match crate::current_round(self.root) {
            Ok(round) => round,
            Err(error) => {
                eprintln!(
                    "⚠️ 记录 wave blocker {}:{} 时获取当前轮失败: {error:#}",
                    blocker.task_id, blocker.class
                );
                return false;
            }
        };
        match record_wave_blocker(self.root, &round, blocker) {
            Ok(path) => {
                eprintln!(
                    "⚠️ wave blocker {}:{} 已记录到 {}",
                    blocker.task_id, blocker.class, path
                );
                true
            }
            Err(error) => {
                eprintln!(
                    "⚠️ 记录 wave blocker {}:{} 失败: {error:#}",
                    blocker.task_id, blocker.class
                );
                false
            }
        }
    }
}
