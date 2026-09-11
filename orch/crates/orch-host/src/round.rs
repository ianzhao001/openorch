//! `orch round`：轮生命周期命令化——open / sign-off / seed-verified / close。
//! r0-r3 开轮收轮均为人肉写 events+BOARD（HANDOFF §6.5）；命令化消除手写 eventId 与漏项
//! （r3 实测漏写 dispatch/DONE.md——design/03 §4.2 步骤 8 由 close 补成结构性保证）。
//! HITL#1（决策 6）：PlanSignedOff 只经显式 sign-off 命令或 open --signed-off 落账，永不隐式。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use orch_core::{fold, read_ledger, EventRecord, TaskState};
use sha2::{Digest, Sha256};

use crate::{binding, board_append, card, current_round, gitx, ledger, plan};

pub struct OpenOutcome {
    pub round: String,
    /// 崩溃窗修复：账本已有 RoundOpened 但 CURRENT-ROUND 未切换，本次仅补指针
    pub repaired: bool,
}

pub struct CloseOutcome {
    pub round: String,
    pub final_main_short: String,
    pub recorded: usize,
    pub total: usize,
    pub unrecorded: Vec<String>,
    /// O1：残留 worktree/分支只提醒不代删（Tier F 客户端 shell 可能驻留其中）
    pub leftover_worktrees: Vec<String>,
    pub leftover_branches: Vec<String>,
}

fn ledger_path(root: &Path, round: &str) -> std::path::PathBuf {
    root.join(format!("coordination/rounds/{round}/events.jsonl"))
}

/// Resolve the contract generation from the unique canonical `RoundOpened`.
/// Historical events omit the marker and therefore return `None`.
pub fn contract_schema_from_events(events: &[EventRecord], round: &str) -> Result<Option<u32>> {
    let matching = events
        .iter()
        .filter(|event| event.kind == "RoundOpened")
        .collect::<Vec<_>>();
    let marked = matching.iter().filter(|event| {
        event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("contractSchemaVersion"))
            .is_some()
    });
    if marked.count() == 0 {
        return Ok(None);
    }
    let [opened] = matching.as_slice() else {
        bail!(
            "round {round} 必须恰有一个 canonical RoundOpened，实得 {}",
            matching.len()
        );
    };
    if opened.actor != "runtime:orch"
        || opened.task_id.is_some()
        || opened.round.as_deref() != Some(round)
    {
        bail!("round {round} 的唯一 RoundOpened envelope 非 canonical");
    }
    if events
        .first()
        .is_none_or(|first| !std::ptr::eq(first, *opened))
    {
        bail!("round {round} 的 marked RoundOpened 必须是 ledger 第一条事件");
    }
    let Some(payload) = opened.payload.as_ref() else {
        return Ok(None);
    };
    let Some(value) = payload.get("contractSchemaVersion") else {
        return Ok(None);
    };
    let version = value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .context("RoundOpened.contractSchemaVersion 必须是 u32 integer")?;
    if version != plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        bail!("RoundOpened.contractSchemaVersion={version} 未建模");
    }
    Ok(Some(version))
}

/// Load and validate one round's durable contract generation.
pub fn contract_schema_at_root(root: &Path, round: &str) -> Result<Option<u32>> {
    let read = match read_ledger(&ledger_path(root, round)) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !read.bad_lines.is_empty() {
        bail!("round {round} ledger 含坏行，拒绝解析 contract schema");
    }
    if !read.events.iter().any(|event| event.kind == "RoundOpened") {
        return Ok(None);
    }
    contract_schema_from_events(&read.events, round)
}

/// Archived verdicts bind to the production validation facts recorded in the
/// ledger.  Historical ROUND-IR bytes must not be re-canonicalized by a newer
/// binary: schema evolution can legitimately change that derived digest.
pub fn archived_binding_ok(
    verdict_revision: u32,
    verdict_digest: &str,
    ledger_validations: &[(u32, String)],
) -> bool {
    ledger_validations
        .iter()
        .any(|(revision, digest)| *revision == verdict_revision && digest == verdict_digest)
}

fn today(ts_rfc3339: &str) -> &str {
    &ts_rfc3339[..ts_rfc3339.len().min(10)]
}

fn now_date() -> String {
    let ts = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    today(&ts).to_string()
}

/// Open a schema 3 round without embedding scheduling or participant policy.
///
/// The CLI uses this entry point for every newly-created round. Historical
/// schemas have no writer in the production crate.
pub fn run_open_v3(root: &Path, id: &str, purpose: &str, force: bool) -> Result<OpenOutcome> {
    crate::reclaim::checked_storage_path(
        &fs::canonicalize(root)?,
        Path::new("coordination/runtime"),
    )?;
    crate::close::with_protocol_transition(root, "orch round open v3", || {
        run_open_locked(root, id, purpose, force)
    })
}

fn prepare_round_storage(root: &Path) -> Result<crate::storage::StoragePermit> {
    match crate::reclaim::latest_maintenance_report(root) {
        Ok(Some(previous)) => {
            let unresolved = previous
                .items
                .iter()
                .filter(|item| {
                    item.disposition == "failed"
                        || (item.disposition == "held"
                            && item.kind != "shared"
                            && item.logical_bytes.is_some_and(|n| n > 0))
                })
                .count();
            if unresolved > 0 || previous.report_error.is_some() {
                eprintln!("previous storage maintenance: {unresolved} unresolved items; retrying authorized safe reclamation");
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!("previous maintenance report unreadable: {error:#}"),
    }
    match crate::reclaim::maintain_storage(root, false) {
        Ok(report) => {
            for line in crate::reclaim::maintenance_summary(&report) {
                eprintln!("{line}");
            }
        }
        Err(error) => {
            eprintln!("storage maintenance failed; fresh admission still decides: {error:#}")
        }
    }
    crate::storage::guard_round_open(root)
}

fn run_open_locked(root: &Path, id: &str, purpose: &str, force: bool) -> Result<OpenOutcome> {
    // 步骤 1：id 校验（惯例 r<N>，其余只 warn 不拒）
    let valid = !id.is_empty()
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid {
        bail!("轮 id 非法: {id:?}（允许 [a-z0-9][a-z0-9-]*）");
    }
    if !(id.starts_with('r') && id[1..].chars().all(|c| c.is_ascii_digit()) && id.len() > 1) {
        eprintln!("⚠️ 轮 id {id:?} 不符合 r<N> 惯例（继续）");
    }

    // 步骤 2：幂等/拒绝语义
    let new_ledger = ledger_path(root, id);
    let existing_ledger = if new_ledger.is_file() {
        let read = read_ledger(&new_ledger)?;
        if !read.bad_lines.is_empty() {
            bail!("部分开轮 ledger 含坏行");
        }
        Some(read)
    } else {
        None
    };
    let already_opened = existing_ledger
        .as_ref()
        .is_some_and(|read| read.events.iter().any(|event| event.kind == "RoundOpened"));
    if existing_ledger
        .as_ref()
        .is_some_and(|read| !read.events.is_empty() && !already_opened)
    {
        bail!("目标 round ledger 非空但缺 RoundOpened，拒绝把非法前缀修成新轮");
    }
    let pointer = current_round(root).ok();
    if already_opened {
        let existing = &existing_ledger.as_ref().expect("already opened").events;
        if existing
            .first()
            .is_none_or(|event| event.kind != "RoundOpened" || event.round.as_deref() != Some(id))
        {
            bail!("RoundOpened 必须是 round ledger 第一条事件");
        }
        if fold(existing).round_closed {
            bail!("round {id} 已关闭，拒绝修复 CURRENT-ROUND 指针");
        }
        let actual = contract_schema_from_events(existing, id)?;
        if actual != Some(plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
            bail!(
                "部分开轮 schema 漂移：期望 contractSchemaVersion={}，实得 {actual:?}",
                plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION
            );
        }
        if pointer.as_deref() == Some(id) {
            bail!("重复开轮：{id} 已开且 CURRENT-ROUND 已指向它（看 orch status）");
        }
        // 崩溃窗：账已落、指针未切——只补指针
        let _space = prepare_round_storage(root)?;
        crate::hooks::ensure_main_guard(root).context("恢复开轮保护 hook 失败")?;
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{id}\n"),
        )?;
        eprintln!("⚠️ 部分开轮已补齐：{id} 账本已有 RoundOpened，本次仅补写 CURRENT-ROUND");
        return Ok(OpenOutcome {
            round: id.to_string(),
            repaired: true,
        });
    }
    if let Some(prev) = &pointer {
        if prev != id {
            let prev_ledger = ledger_path(root, prev);
            let prev_closed =
                prev_ledger.is_file() && fold(&read_ledger(&prev_ledger)?.events).round_closed;
            if !prev_closed && !force {
                bail!("上一轮 {prev} 未收轮（RoundClosed 缺失）——先 orch round close，或 --force 强开");
            }
        }
    }

    let _space = prepare_round_storage(root)?;
    crate::hooks::ensure_main_guard(root)
        .context("开轮前安装主仓 reference-transaction guard 失败")?;

    // 步骤 3：轮目录骨架（O6：git 不跟踪空目录，协议方写入前自愈）
    for sub in [
        "tasks", "seeds", "reports", "reviews", "evidence", "dispatch",
    ] {
        fs::create_dir_all(root.join(format!("coordination/rounds/{id}/{sub}")))?;
    }

    // 步骤 4：落账（单次 append 同锁原子）
    let mut payload = serde_json::json!({
        "purpose": purpose,
        "contractSchemaVersion": plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION,
    });
    if force {
        if let Some(prev) = &pointer {
            payload["forcedOverUnclosed"] = serde_json::json!(prev);
        }
    }
    let events = vec![ledger::event(
        "RoundOpened",
        "runtime:orch",
        None,
        Some(id),
        payload,
    )];
    ledger::append(root, id, &events)?;

    // 步骤 5：账落成功后才切指针（E8）
    fs::create_dir_all(root.join("coordination/runtime"))?;
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{id}\n"),
    )?;

    // 步骤 6：BOARD 开版段（对齐 r0-r3 版式）
    board_append(
        root,
        &format!(
            "\n## {id}（{purpose}）\n\n- {} · {id} 开版（orch round open）：{purpose}。\n",
            now_date()
        ),
    )?;
    Ok(OpenOutcome {
        round: id.to_string(),
        repaired: false,
    })
}

/// `orch round sign-off`：PlanSignedOff（actor=user）——HITL#1 显式命令面
pub fn run_sign_off(root: &Path, note: Option<&str>) -> Result<String> {
    // Keep pure argument rejection ahead of the operational lease file so an
    // invalid command remains entirely side-effect free.
    if note.is_some_and(|value| value.trim().is_empty()) {
        bail!("PlanSignedOff note 不得为空白");
    }
    crate::close::with_protocol_transition(root, "orch round sign-off", || {
        run_sign_off_locked(root, note)
    })
}

fn run_sign_off_locked(root: &Path, note: Option<&str>) -> Result<String> {
    let round = current_round(root)?;
    let lr = read_ledger(&ledger_path(root, &round))?;
    if !lr.bad_lines.is_empty() {
        bail!("轮 {round} 账本含坏行，拒绝签核");
    }
    let validation = plan::require_validated_round_ir(root, &round, &lr.events)?;
    let pending_reverify = plan::pending_reverify_tasks(
        &lr.events,
        &round,
        validation.persisted_revision,
        &validation.persisted_digest,
    )?;
    if !pending_reverify.is_empty() {
        bail!(
            "轮 {round} 当前 IR revision={} 尚有 reverifyTasks 未在本 revision 重跑 SeedOracleVerified: {}",
            validation.persisted_revision,
            pending_reverify.join(", ")
        );
    }
    if plan::matching_user_plan_signoff(
        &lr.events,
        &round,
        validation.persisted_revision,
        &validation.persisted_digest,
    )? {
        bail!(
            "轮 {round} 当前 IR revision={} digest={} 已签核",
            validation.persisted_revision,
            validation.persisted_digest
        );
    }
    let payload = plan::plan_signed_off_payload(
        note.unwrap_or("用户签核（orch round sign-off）"),
        validation.persisted_revision,
        &validation.persisted_digest,
    )?;
    let appended = ledger::append_checked(root, &round, |events| {
        let fresh = plan::require_validated_round_ir(root, &round, events)?;
        if fresh.persisted_revision != validation.persisted_revision
            || fresh.persisted_digest != validation.persisted_digest
        {
            bail!("PlanSignedOff append 前 validated IR 漂移");
        }
        let fresh_pending = plan::pending_reverify_tasks(
            events,
            &round,
            fresh.persisted_revision,
            &fresh.persisted_digest,
        )?;
        if !fresh_pending.is_empty() {
            bail!(
                "PlanSignedOff append 前 reverifyTasks 未完成: {}",
                fresh_pending.join(", ")
            );
        }
        let exact = plan::matching_user_plan_signoff_positions(
            events,
            &round,
            fresh.persisted_revision,
            &fresh.persisted_digest,
        )?;
        if !exact.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(&round),
            payload.clone(),
        )])
    })?;
    if appended == 0 {
        bail!(
            "轮 {round} 当前 IR revision={} digest={} 已签核",
            validation.persisted_revision,
            validation.persisted_digest
        );
    }
    Ok(round)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedVerificationDecision {
    First,
    AlreadyVerified { event_id: String },
    Revision { supersedes_event_id: String },
}

fn normalize_current_seeds(seeds: &[(String, String)]) -> Result<BTreeMap<String, String>> {
    let mut normalized = BTreeMap::new();
    for (target, sha256) in seeds {
        if target.is_empty() || sha256.is_empty() {
            bail!("当前 seed set 含空 target/sha256，拒绝判定");
        }
        if normalized.insert(target.clone(), sha256.clone()).is_some() {
            bail!("当前 seed set 含重复 target: {target}");
        }
    }
    Ok(normalized)
}

fn normalize_historical_seeds(event: &EventRecord) -> Result<BTreeMap<String, String>> {
    let seeds = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("seeds"))
        .and_then(serde_json::Value::as_array)
        .with_context(|| {
            format!(
                "历史 SeedOracleVerified {} 的 payload.seeds 缺失或非数组",
                event.event_id
            )
        })?;
    let mut normalized = BTreeMap::new();
    for (index, seed) in seeds.iter().enumerate() {
        let target = seed
            .get("target")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| {
                format!(
                    "历史 SeedOracleVerified {} 的 seeds[{index}].target 缺失、类型错误或为空",
                    event.event_id
                )
            })?;
        let sha256 = seed
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| {
                format!(
                    "历史 SeedOracleVerified {} 的 seeds[{index}].sha256 缺失、类型错误或为空",
                    event.event_id
                )
            })?;
        if normalized
            .insert(target.to_string(), sha256.to_string())
            .is_some()
        {
            bail!(
                "历史 SeedOracleVerified {} 含重复 target: {target}",
                event.event_id
            );
        }
    }
    Ok(normalized)
}

/// Classify the current seed bytes against the latest task-scoped oracle event.
///
/// Both sides are normalized as complete target → SHA maps. Malformed history
/// and duplicate targets are rejected rather than treated as a new revision.
pub fn classify_seed_verification(
    events: &[EventRecord],
    task_id: &str,
    current_seeds: &[(String, String)],
) -> Result<SeedVerificationDecision> {
    let current = normalize_current_seeds(current_seeds)?;
    let Some(latest) = events.iter().rev().find(|event| {
        event.kind == "SeedOracleVerified" && event.task_id.as_deref() == Some(task_id)
    }) else {
        return Ok(SeedVerificationDecision::First);
    };
    let historical = normalize_historical_seeds(latest)?;
    if historical == current {
        Ok(SeedVerificationDecision::AlreadyVerified {
            event_id: latest.event_id.clone(),
        })
    } else {
        Ok(SeedVerificationDecision::Revision {
            supersedes_event_id: latest.event_id.clone(),
        })
    }
}

/// `orch round seed-verified <task>`：核对种子 SHA + **真跑 oracle 预验**（M2 命令化：
/// 隔离 worktree 落位种子跑测试门，O5 机器化判据）。`--record-only` 降级为仅记录（逃生舱）。
pub fn run_seed_verified(
    root: &Path,
    task_id: &str,
    expected_red: &str,
    record_only: bool,
    allow_file_passed: usize,
) -> Result<(String, Option<crate::oracle::Measured>)> {
    crate::close::with_protocol_transition(root, "orch round seed-verified", || {
        run_seed_verified_locked(root, task_id, expected_red, record_only, allow_file_passed)
    })
}

fn run_seed_verified_locked(
    root: &Path,
    task_id: &str,
    expected_red: &str,
    record_only: bool,
    allow_file_passed: usize,
) -> Result<(String, Option<crate::oracle::Measured>)> {
    let round = current_round(root)?;
    card::validate_task_id(task_id)?;
    let ledger_file = ledger_path(root, &round);
    let lr = read_ledger(&ledger_file)?;
    if !lr.bad_lines.is_empty() {
        bail!("seed-verified 拒绝坏账本");
    }
    let validated = crate::plan::require_validated_round_ir(root, &round, &lr.events)?;
    let ir_task = validated
        .candidate
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("validated ROUND-IR 不含 task {task_id}"))?;
    if ir_task.seed_protocol != "seeded-red" || !ir_task.has_seeds {
        bail!("task {task_id} 不是 validated seeded-red task");
    }
    let c = crate::plan::load_bound_task_card(root, &round, task_id, &validated)?;
    crate::oracle::validate_seed_paths(root, &c)?;
    if c.meta.seeds.is_empty() {
        bail!("任务卡 {task_id} 无 seeds——seed-verified 无意义（verify-only/pure-spec 档不需要）");
    }
    // Schema-v2 syntax and card declaration are proven before preverify can
    // create a worktree or run a gate. `--record-only` cannot manufacture a
    // measured identity/proof and is therefore a live-event downgrade.
    let loaded_binding = binding::load(root)?;
    crate::oracle::validate_expected_red_syntax(
        c.meta.red_form.as_deref(),
        &loaded_binding.oracle.dialect,
        expected_red,
        record_only,
    )
    .map_err(anyhow::Error::msg)?;
    // 逐 seed 读源文件算 SHA，与卡内声明比对（不符即 bail——预验的对象必须是将被搬运的字节）
    let mut current_seeds = Vec::new();
    let mut seeds_payload = Vec::new();
    for s in &c.meta.seeds {
        let bytes = crate::oracle::read_bound_seed_bytes(root, &s.src)?;
        let sha = hex::encode(Sha256::digest(&bytes));
        let declared = s
            .sha256
            .as_ref()
            .with_context(|| format!("种子 {} 缺少卡片 sha256 声明", s.src))?;
        if declared != &sha {
            bail!("种子 {} SHA 不符：卡声明 {declared} ≠ 实测 {sha}", s.src);
        }
        current_seeds.push((s.target.clone(), sha.clone()));
        seeds_payload.push(serde_json::json!({ "target": s.target, "sha256": sha }));
    }
    let decision = classify_seed_verification(&lr.events, task_id, &current_seeds)?;
    let requires_current_reverify = crate::plan::pending_reverify_tasks(
        &lr.events,
        &round,
        validated.persisted_revision,
        &validated.persisted_digest,
    )?
    .iter()
    .any(|pending| pending == task_id);
    match &decision {
        SeedVerificationDecision::AlreadyVerified { event_id } if !requires_current_reverify => {
            bail!(
                "{task_id} 的相同 seed set 已由 SeedOracleVerified {event_id} 预验——重复预验拒绝"
            );
        }
        SeedVerificationDecision::Revision { .. } if record_only => {
            bail!("{task_id} 是 seed revision——禁止 --record-only，必须重新执行完整 oracle 预验");
        }
        SeedVerificationDecision::First | SeedVerificationDecision::Revision { .. } => {}
        SeedVerificationDecision::AlreadyVerified { .. } => {}
    }
    if requires_current_reverify && record_only {
        bail!("{task_id} 属于当前 revision 的 reverifyTasks——禁止 --record-only，必须重新执行完整 oracle 预验");
    }
    let before_oracle = read_ledger(&ledger_file)?;
    if !before_oracle.bad_lines.is_empty() {
        bail!("seed oracle 临界前账本出现坏行");
    }
    let fresh_validated =
        crate::plan::require_validated_round_ir(root, &round, &before_oracle.events)?;
    if fresh_validated.persisted_revision != validated.persisted_revision
        || fresh_validated.persisted_digest != validated.persisted_digest
        || !fresh_validated
            .candidate
            .tasks
            .iter()
            .any(|task| task.id == task_id)
    {
        bail!("seed oracle 临界前 validated IR/task membership 漂移");
    }
    // One gate run produces both the legacy measured projection and the
    // canonical schema-v2 identity. Expected-red is recomputed from these
    // facts before payload construction or any ledger/WAL append.
    let mut observation = crate::oracle::preverify_observation(root, &c, allow_file_passed)?;
    let red_form = c
        .meta
        .red_form
        .as_deref()
        .context("schema-v2 card redForm disappeared after syntax validation")?;
    if red_form == "assertion" && observation.measured.red_form.is_none() {
        observation.measured.red_form = Some("assertion".into());
    }
    let expected_red_proof =
        crate::oracle::prove_expected_red(red_form, expected_red, &observation)
            .map_err(anyhow::Error::msg)?;
    let measured = observation.measured.clone();
    let mut payload = serde_json::json!({
        "oracleSchemaVersion": 2,
        "expectedRed": expected_red,
        "expectedRedProof": expected_red_proof,
        "seeds": seeds_payload,
        "irRevision": validated.persisted_revision,
        "measured": serde_json::to_value(&observation)?,
    });
    if let SeedVerificationDecision::Revision {
        supersedes_event_id,
    } = &decision
    {
        payload["supersedesSeedOracleEventId"] = serde_json::json!(supersedes_event_id);
    }
    let expected_revision = validated.persisted_revision;
    let expected_digest = validated.persisted_digest;
    let payload_for_append = payload.clone();
    ledger::append_checked(root, &round, |events| {
        let fresh = crate::plan::require_validated_round_ir(root, &round, events)?;
        if fresh.persisted_revision != expected_revision
            || fresh.persisted_digest != expected_digest
            || !fresh.candidate.tasks.iter().any(|task| task.id == task_id)
        {
            bail!("SeedOracleVerified append 前 validated IR/task membership 漂移");
        }
        if requires_current_reverify
            && !crate::plan::pending_reverify_tasks(
                events,
                &round,
                expected_revision,
                &expected_digest,
            )?
            .iter()
            .any(|pending| pending == task_id)
        {
            bail!("SeedOracleVerified append 前本 revision reverify 已由并发事件满足");
        }
        let fresh_decision = classify_seed_verification(events, task_id, &current_seeds)?;
        if fresh_decision != decision {
            bail!("SeedOracleVerified append 前 seed history 漂移");
        }
        Ok(vec![ledger::event(
            "SeedOracleVerified",
            "planner",
            Some(task_id),
            Some(&round),
            payload_for_append.clone(),
        )])
    })?;
    Ok((round, Some(measured)))
}

/// `orch round close`：核账（普通模式全 Recorded；force 仅豁免未收任务）→ DONE.md → RoundClosed → BOARD → O1
pub fn run_close(root: &Path, force: bool, note: Option<&str>) -> Result<CloseOutcome> {
    crate::close::with_protocol_transition(root, "orch round close", || {
        run_close_locked(root, force, note)
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchivedMergeStartedBinding {
    verdict_event_id: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchivedVerdictBinding {
    ir_revision: u32,
    validation_digest: String,
    main_head_sha: String,
    verdict: String,
}

fn production_ledger_validations(
    events: &[EventRecord],
    round: &str,
) -> Result<Vec<(u32, String)>> {
    let mut validations = Vec::new();
    for event in events.iter().filter(|event| {
        event.kind == "TaskValidated"
            && event.actor == "runtime:orch"
            && event.task_id.is_none()
            && event.round.as_deref() == Some(round)
    }) {
        let payload = crate::plan::decode_task_validated(event)?;
        let production_digest = payload.validation_digest.len() == 64
            && payload
                .validation_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if production_digest {
            validations.push((payload.ir_revision, payload.validation_digest));
        }
    }
    Ok(validations)
}

fn validate_archived_binding_for_close(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<()> {
    let starts = events
        .iter()
        .filter(|event| {
            event.kind == "MergeStarted"
                && event.actor == "runtime:orch"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && !crate::close::merge_started_is_stale(events, task_id, event)
        })
        .collect::<Vec<_>>();
    if starts.len() != 1 {
        bail!("round close 要求 task {task_id} 恰好一条 canonical MergeStarted");
    }
    let started: ArchivedMergeStartedBinding = serde_json::from_value(
        starts[0]
            .payload
            .clone()
            .context("archived MergeStarted 缺 payload")?,
    )
    .context("archived MergeStarted payload 非 canonical")?;
    let verdicts = events
        .iter()
        .filter(|event| event.event_id == started.verdict_event_id)
        .collect::<Vec<_>>();
    if verdicts.len() != 1 {
        bail!("archived MergeStarted.verdictEventId 未精确绑定唯一事件");
    }
    let verdict_event = verdicts[0];
    if verdict_event.kind != "VerdictIssued"
        || verdict_event.actor != "verifier:root"
        || verdict_event.task_id.as_deref() != Some(task_id)
        || verdict_event.round.as_deref() != Some(round)
    {
        bail!("archived verdict envelope 非 canonical verifier:root tuple");
    }
    let verdict: ArchivedVerdictBinding = serde_json::from_value(
        verdict_event
            .payload
            .clone()
            .context("archived verdict 缺 payload")?,
    )
    .context("archived verdict payload 非 canonical")?;
    if verdict.verdict != "PASS" {
        bail!("archived verdict 必须为 PASS");
    }
    let validations = production_ledger_validations(events, round)?;
    if !archived_binding_ok(
        verdict.ir_revision,
        &verdict.validation_digest,
        &validations,
    ) {
        bail!(
            "archived verdict (revision={}, digest={}) 不在 production TaskValidated 账本真值中",
            verdict.ir_revision,
            verdict.validation_digest
        );
    }
    let historical_ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    gitx::show_bytes(root, &verdict.main_head_sha, &historical_ir_rel).with_context(|| {
        format!(
            "archived verdict mainHeadSha={} 不含 committed ROUND-IR path {historical_ir_rel}",
            verdict.main_head_sha
        )
    })?;
    Ok(())
}

fn validate_archived_record_chain_for_close(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<()> {
    match crate::verify::validate_archived_record_chain(root, round, task_id, events) {
        Ok(()) => Ok(()),
        Err(error)
            if error.chain().any(|cause| {
                cause.to_string()
                    == "archived root PASS revision/digest 未绑定 mainHead committed ROUND-IR"
            }) =>
        {
            validate_archived_binding_for_close(root, round, task_id, events)
        }
        Err(error) => Err(error),
    }
}

fn close_active_task_states(
    events: &[orch_core::EventRecord],
    round: &str,
    ir_tasks: &std::collections::BTreeSet<String>,
    projection: &orch_core::RoundProjection,
) -> Result<std::collections::BTreeMap<String, Option<TaskState>>> {
    let validation_position = events
        .iter()
        .rposition(|event| event.kind == "TaskValidated" && event.round.as_deref() == Some(round))
        .context("round close 缺 current TaskValidated")?;
    let validation_projection = fold(&events[..=validation_position]);
    let terminal_cleanup = |kind: &str| {
        matches!(
            kind,
            "ManagedWakeTerminated"
                | "ManagedWakeAttachState"
                | "WorkspaceReleased"
                | "SiteRetired"
                | "CostSampled"
        )
    };

    for (task_id, current) in &projection.tasks {
        if ir_tasks.contains(task_id) {
            continue;
        }
        let signed_out_state = validation_projection
            .tasks
            .get(task_id)
            .and_then(|task| task.state)
            .with_context(|| {
                format!("round close ledger extra task {task_id} 在 current validation 前没有状态")
            })?;
        if !matches!(
            signed_out_state,
            TaskState::Blocked | TaskState::ChangesRequested
        ) {
            bail!(
                "round close ledger extra task {task_id} 未在 current validation 前终态化: {signed_out_state}"
            );
        }
        if let Some(event) = events[validation_position + 1..].iter().find(|event| {
            event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id.as_str())
                && !terminal_cleanup(&event.kind)
        }) {
            bail!(
                "round close ledger extra task {task_id} 在 current validation 后出现非清理事件 {}:{}",
                event.kind,
                event.event_id
            );
        }
        if current.state != Some(signed_out_state) {
            bail!("round close ledger extra task {task_id} 在 current validation 后状态漂移");
        }
    }

    ir_tasks
        .iter()
        .map(|task_id| {
            projection
                .tasks
                .get(task_id)
                .map(|task| (task_id.clone(), task.state))
                .with_context(|| {
                    format!("round close active IR task {task_id} 缺 ledger projection")
                })
        })
        .collect()
}

fn run_close_locked(root: &Path, force: bool, note: Option<&str>) -> Result<CloseOutcome> {
    if force && note.is_none_or(|value| value.trim().is_empty()) {
        bail!("round close --force 必须提供非空 note，逐条记录 FORCE-POLICY 五问结论");
    }
    let round = current_round(root)?;
    let lr = read_ledger(&ledger_path(root, &round))?;
    if !lr.bad_lines.is_empty() {
        bail!("round close 拒绝坏账本");
    }
    crate::ledger::validate_runtime_event_history_v1_at_root(root, &lr.events, &round)
        .context("round close runtime V1 event contract 非 canonical")?;
    let active = crate::plan::require_active_round_ir(root, &round, &lr.events)?;
    let p = fold(&lr.events);
    if p.round_closed {
        bail!("轮 {round} 已收轮（RoundClosed 在账）");
    }

    // 步骤 1：核账
    let ir_tasks = active
        .candidate
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let task_states = close_active_task_states(&lr.events, &round, &ir_tasks, &p)?;
    let total = ir_tasks.len();
    let unrecorded: Vec<String> = p
        .tasks
        .iter()
        .filter(|(id, t)| ir_tasks.contains(*id) && t.state != Some(TaskState::Recorded))
        .map(|(id, t)| {
            format!(
                "{id}({})",
                t.state.map(|s| s.to_string()).unwrap_or_else(|| "?".into())
            )
        })
        .collect();
    if !unrecorded.is_empty() && !force {
        bail!(
            "拒绝收轮：{} 个 active IR 任务未 Recorded —— {}",
            unrecorded.len(),
            unrecorded.join(", ")
        );
    }
    let recorded = total - unrecorded.len();
    let recorded_tasks = task_states
        .iter()
        .filter(|(_, state)| **state == Some(TaskState::Recorded))
        .map(|(task_id, _)| task_id.as_str())
        .collect::<Vec<_>>();
    let unresolved_panels = crate::ledger::unresolved_review_panels_v1(&lr.events, &round)?;
    if let Some((panel, task, attempt)) = unresolved_panels.first() {
        bail!(
            "round close 拒绝未闭合 review panel（--force 不豁免活审查）: panel={panel} task={task} attempt={attempt}"
        );
    }
    for task_id in &recorded_tasks {
        validate_archived_record_chain_for_close(root, &round, task_id, &lr.events)
            .with_context(|| format!("round close task {task_id} archived record chain 非法"))?;
    }
    let frozen_contract_supersessions_by_initiator =
        crate::verify::validated_frozen_contract_supersession_counts(root, &round, &lr.events)
            .context("round close FrozenContractSuperseded effective chain 非法")?;

    // Immediately before the first close side effect, re-read the permit and
    // exact terminal task set.  CLI preflight is not a durable host permit.
    let before_done = read_ledger(&ledger_path(root, &round))?;
    if !before_done.bad_lines.is_empty() {
        bail!("写 DONE 前账本出现坏行");
    }
    crate::ledger::validate_runtime_event_history_v1_at_root(root, &before_done.events, &round)
        .context("写 DONE 前 runtime V1 event contract 漂移")?;
    let fresh_active = crate::plan::require_active_round_ir(root, &round, &before_done.events)?;
    if fresh_active.persisted_revision != active.persisted_revision
        || fresh_active.persisted_digest != active.persisted_digest
    {
        bail!("写 DONE 前 active IR 漂移");
    }
    let fresh_projection = fold(&before_done.events);
    let fresh_states =
        close_active_task_states(&before_done.events, &round, &ir_tasks, &fresh_projection)?;
    if fresh_states != task_states {
        bail!("写 DONE 前 task set/state 漂移");
    }
    if crate::ledger::unresolved_review_panels_v1(&before_done.events, &round)? != unresolved_panels
    {
        bail!("写 DONE 前 review panel closure 漂移");
    }
    for task_id in &recorded_tasks {
        validate_archived_record_chain_for_close(root, &round, task_id, &before_done.events)
            .with_context(|| format!("写 DONE 前 task {task_id} archived record chain 非法"))?;
    }
    let fresh_frozen_counts = crate::verify::validated_frozen_contract_supersession_counts(
        root,
        &round,
        &before_done.events,
    )?;
    if fresh_frozen_counts != frozen_contract_supersessions_by_initiator {
        bail!("写 DONE 前 FrozenContractSuperseded effective counts 漂移");
    }

    // 步骤 2：轮终信号 DONE.md（design/03 §4.2 步骤 8 + O4 小结落盘路径；已在=崩溃窗恢复，跳过重写）
    let final_main = gitx::rev_parse(root, "main")?;
    let short = gitx::short(&final_main).to_string();
    let dispatch_dir = root.join(format!("coordination/rounds/{round}/dispatch"));
    fs::create_dir_all(&dispatch_dir)?;
    let done = dispatch_dir.join("DONE.md");
    if !done.is_file() {
        fs::write(&done, format!(
            "# ROUND {round} COMPLETE\n\n本轮已收口（main = `{short}`；任务 {recorded}/{total} Recorded）。\n\
             root planner 请完成本轮 handoff 与收轮核账；下一轮须重新规划、签核并显式 dispatch。\n\
             本文件不触发后台等待、RESUME 或自动跨轮续接。\n"
        ))?;
    }

    // 步骤 3：落账（动作成功后写真值——E8；finalMain 存全 SHA）
    let frozen_contract_supersession_count = frozen_contract_supersessions_by_initiator
        .values()
        .sum::<u64>();
    let close_payload = serde_json::json!({
        "finalMain": final_main,
        "note": note.unwrap_or(""),
        "unrecorded": unrecorded,
        "forced": force,
        "frozenContractSupersessionsByInitiator": frozen_contract_supersessions_by_initiator.clone(),
        "frozenContractSupersessionCount": frozen_contract_supersession_count
    });
    ledger::append_checked(root, &round, |events| {
        crate::ledger::validate_runtime_event_history_v1_at_root(root, events, &round)
            .context("RoundClosed append 前 runtime V1 event contract 漂移")?;
        let current = crate::plan::require_active_round_ir(root, &round, events)?;
        if current.persisted_revision != active.persisted_revision
            || current.persisted_digest != active.persisted_digest
            || gitx::rev_parse(root, "main")? != final_main
        {
            bail!("RoundClosed append 前 active IR/main 漂移");
        }
        let projection = fold(events);
        let current_states = close_active_task_states(events, &round, &ir_tasks, &projection)?;
        if current_states != task_states {
            bail!("RoundClosed append 前 task set/state 漂移");
        }
        if crate::ledger::unresolved_review_panels_v1(events, &round)? != unresolved_panels {
            bail!("RoundClosed append 前 review panel closure 漂移");
        }
        for task_id in &recorded_tasks {
            validate_archived_record_chain_for_close(root, &round, task_id, events).with_context(
                || format!("RoundClosed append 前 task {task_id} archived record chain 非法"),
            )?;
        }
        let current_frozen_counts =
            crate::verify::validated_frozen_contract_supersession_counts(root, &round, events)?;
        if current_frozen_counts != frozen_contract_supersessions_by_initiator {
            bail!("RoundClosed append 前 FrozenContractSuperseded effective counts 漂移");
        }
        if projection.round_closed {
            return Ok(Vec::new());
        }
        Ok(vec![ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some(&round),
            close_payload.clone(),
        )])
    })?;

    // 步骤 4：BOARD 收轮行
    board_append(root, &format!(
        "- {} · **{round} 收轮**（orch round close）：main 终态 `{short}`；任务 {recorded}/{total} Recorded{}{}。\n",
        now_date(),
        if force { "；forced=true" } else { "" },
        note.map(|n| format!("；{n}")).unwrap_or_default()
    ))?;

    // 步骤 5：O1 残留现场盘点（只提醒不代删）
    let mut leftover_worktrees = Vec::new();
    if let Ok(entries) = fs::read_dir(root.join(".worktrees")) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                leftover_worktrees.push(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    let leftover_branches: Vec<String> = p
        .tasks
        .keys()
        .filter(|id| gitx::branch_exists(root, &format!("task/{id}")))
        .map(|id| format!("task/{id}"))
        .collect();

    Ok(CloseOutcome {
        round,
        final_main_short: short,
        recorded,
        total,
        unrecorded,
        leftover_worktrees,
        leftover_branches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // id 校验规则的最小单测（不触文件系统）
    fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }

    #[test]
    fn round_id_rules() {
        assert!(valid_id("r4"));
        assert!(valid_id("r10"));
        assert!(valid_id("hotfix-1"));
        assert!(!valid_id(""));
        assert!(!valid_id("-r4"));
        assert!(!valid_id("r4/x"));
        assert!(!valid_id("r 4"));
    }

    #[test]
    fn schema_evolution_replay_uses_each_archived_ledger_pair() {
        let old_digest = "a".repeat(64);
        let new_digest = "b".repeat(64);
        let recomputed_old_bytes_with_new_schema = "c".repeat(64);
        let events = vec![
            ledger::event(
                "TaskValidated",
                "runtime:orch",
                None,
                Some("r50"),
                crate::plan::task_validated_payload(1, &old_digest),
            ),
            ledger::event(
                "TaskValidated",
                "runtime:orch",
                None,
                Some("r50"),
                crate::plan::task_validated_payload(2, &new_digest),
            ),
        ];
        let validations = production_ledger_validations(&events, "r50").unwrap();

        assert!(archived_binding_ok(1, &old_digest, &validations));
        assert!(archived_binding_ok(2, &new_digest, &validations));
        assert!(!archived_binding_ok(
            1,
            &recomputed_old_bytes_with_new_schema,
            &validations
        ));
        assert!(!archived_binding_ok(1, &new_digest, &validations));
    }

    fn close_event(kind: &str, task_id: Option<&str>) -> orch_core::EventRecord {
        let payload = match kind {
            "DispatchIssued" => serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": format!("{}-A0001", task_id.unwrap()),
                "attemptNo": 1,
                "baseSha": "a".repeat(40)
            }),
            "AttemptBlocked" => serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": format!("{}-A0001", task_id.unwrap()),
                "attemptNo": 1
            }),
            _ => serde_json::json!({}),
        };
        ledger::event(kind, "runtime:orch", task_id, Some("r-close"), payload)
    }

    fn replacement_events(post_validation_kind: Option<&str>) -> Vec<orch_core::EventRecord> {
        let mut events = vec![
            close_event("DispatchIssued", Some("B-old")),
            close_event("AttemptBlocked", Some("B-old")),
            close_event("TaskPlanned", Some("B-new")),
            close_event("TaskValidated", None),
        ];
        if let Some(kind) = post_validation_kind {
            events.push(close_event(kind, Some("B-old")));
        }
        events
    }

    #[test]
    fn close_accepts_only_terminal_signed_out_replacements() {
        let events = replacement_events(Some("ManagedWakeTerminated"));
        let projection = fold(&events);
        let active = std::collections::BTreeSet::from(["B-new".to_string()]);
        let states = close_active_task_states(&events, "r-close", &active, &projection).unwrap();
        assert_eq!(states.len(), 1);
        assert!(states.contains_key("B-new"));
        assert!(!states.contains_key("B-old"));
    }

    #[test]
    fn close_rejects_a_signed_out_task_that_runs_again() {
        let events = replacement_events(Some("DispatchIssued"));
        let projection = fold(&events);
        let active = std::collections::BTreeSet::from(["B-new".to_string()]);
        let error = close_active_task_states(&events, "r-close", &active, &projection)
            .unwrap_err()
            .to_string();
        assert!(error.contains("非清理事件 DispatchIssued"), "{error}");
    }

    #[test]
    fn close_rejects_a_nonterminal_ledger_extra() {
        let events = vec![
            close_event("DispatchIssued", Some("B-old")),
            close_event("TaskPlanned", Some("B-new")),
            close_event("TaskValidated", None),
        ];
        let projection = fold(&events);
        let active = std::collections::BTreeSet::from(["B-new".to_string()]);
        let error = close_active_task_states(&events, "r-close", &active, &projection)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("未在 current validation 前终态化"),
            "{error}"
        );
    }
}
