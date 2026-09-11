//! oracle 预验命令化 + 机检级先红复跑（M2 首切片，design/09 M2 + errata E12/O5）。
//! - 预验（`orch round seed-verified`）：隔离 detached worktree 落位种子真跑测试门，
//!   O5 机器化判据 = 「种子文件内 passed ≤ 阈值(默认 0，无空洞位) ∧ 总红数==种子文件红数(基线不受扰)」。
//! - 先红复跑（收取机检阶段）：seed commit 处复跑测试门，与账本预验实测比对——
//!   伪造先红在**零模型成本**的机检层拦截（E12 兜底，不再单靠 verifier）。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{binding, card, gate, gitx, ledger};

const GATE_EXECUTED_SCHEMA: (&str, &[&str]) = (
    "GateExecuted",
    &[
        "commandRef",
        "phase",
        "gateRunId",
        "exitCode",
        "durationMs",
        "subjectTreeSha",
        "logSha256",
        "logBytes",
        "toolchainDigest",
        "environmentDigest",
    ],
);
/// vitest 汇总行计数（`Tests  5 failed | 33 passed (38)` / `Tests  43 passed (43)`）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SuiteCounts {
    pub failed: usize,
    pub passed: usize,
    pub total: usize,
}

/// 单文件行计数（`❯ test/x.test.ts (5 tests | 5 failed)` / `✓ test/x.test.ts (5 tests)`）
#[derive(Debug, Clone, Copy)]
pub struct FileCounts {
    pub tests: usize,
    pub failed: usize,
}

/// 将任务卡里的路径收窄为可移植的仓库相对路径。
///
/// 只接受 `/` 分隔的普通 component；绝对路径、`.`、`..`、重复分隔符和尾随
/// 分隔符一律拒绝，避免 `Path::join` 在 absolute path 上丢弃可信 base，或经
/// parent traversal 逃出仓库/worktree。
fn canonical_repo_relative(raw: &str, field: &str) -> Result<PathBuf> {
    if raw.is_empty() {
        bail!("seed {field} 不能为空");
    }
    if raw.contains('\\') {
        bail!("seed {field} 只接受可移植的 '/' 分隔符: {raw:?}");
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        bail!("seed {field} 必须是仓库相对路径，拒绝绝对路径: {raw:?}");
    }

    let mut parts = Vec::new();
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            bail!("seed {field} 含非规范 component（只允许普通相对路径）: {raw:?}");
        };
        let part = part
            .to_str()
            .with_context(|| format!("seed {field} 含非 UTF-8 component: {raw:?}"))?;
        parts.push(part);
        normalized.push(part);
    }
    if parts.is_empty() || parts.join("/") != raw {
        bail!("seed {field} 不是规范仓库相对路径: {raw:?}");
    }
    Ok(normalized)
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("解析 {label} 真实路径失败: {}", path.display()))?;
    if !fs::metadata(&canonical)
        .with_context(|| format!("读取 {label} 元数据失败: {}", canonical.display()))?
        .is_dir()
    {
        bail!("{label} 不是目录: {}", canonical.display());
    }
    Ok(canonical)
}

/// 在可信 base 下逐 component 做 `symlink_metadata`，拒绝任一中间链接。
fn checked_regular_source(
    base: &Path,
    relative: &Path,
    display: &str,
) -> Result<(PathBuf, fs::Metadata)> {
    let components = relative.components().collect::<Vec<_>>();
    let mut current = base.to_path_buf();
    let mut final_metadata = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            bail!("seed src 内部校验遇到非规范 component: {display:?}");
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("读取 seed src 失败: {display} ({})", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "seed src 禁止符号链接（含父目录）: {display} ({})",
                current.display()
            );
        }
        if index + 1 < components.len() && !metadata.is_dir() {
            bail!(
                "seed src 父 component 不是目录: {display} ({})",
                current.display()
            );
        }
        final_metadata = Some(metadata);
    }

    let metadata = final_metadata.context("seed src 缺路径 component")?;
    if !metadata.is_file() {
        bail!(
            "seed src 必须是 regular file: {display} ({})",
            current.display()
        );
    }
    let resolved = fs::canonicalize(&current)
        .with_context(|| format!("解析 seed src 真实路径失败: {display}"))?;
    if !resolved.starts_with(base) {
        bail!(
            "seed src 解析后逃出仓库: {display} → {}",
            resolved.display()
        );
    }
    Ok((current, metadata))
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

fn read_checked_seed(
    base: &Path,
    relative: &Path,
    display: &str,
) -> Result<(Vec<u8>, fs::Permissions)> {
    let (path, expected) = checked_regular_source(base, relative, display)?;
    let mut file = fs::File::open(&path)
        .with_context(|| format!("打开 seed src 失败: {display} ({})", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("读取已打开 seed src 元数据失败: {display}"))?;
    if !opened.is_file() || !same_file_identity(&expected, &opened) {
        bail!("seed src 在检查与打开之间发生替换: {display}");
    }

    // 打开后再走一次路径校验，缩小父目录被替换成 symlink 的 TOCTOU 窗口。
    let (_, after_open) = checked_regular_source(base, relative, display)?;
    if !same_file_identity(&opened, &after_open) {
        bail!("seed src 在打开后发生路径替换: {display}");
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("读取 seed src 内容失败: {display}"))?;
    let handle_after = file
        .metadata()
        .with_context(|| format!("复核已打开 seed src 元数据失败: {display}"))?;
    let (_, path_after) = checked_regular_source(base, relative, display)?;
    if !same_file_identity(&opened, &handle_after)
        || !same_file_identity(&handle_after, &path_after)
        || handle_after.len() != bytes.len() as u64
    {
        bail!("seed src 读取期间 bytes/inode/path 发生漂移: {display}");
    }
    Ok((bytes, opened.permissions()))
}

pub(crate) fn read_bound_seed_bytes(root: &Path, source: &str) -> Result<Vec<u8>> {
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let relative = canonical_repo_relative(source, "src")?;
    read_checked_seed(&canonical_root, &relative, source).map(|(bytes, _)| bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_full_commit_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Prove that two texts differ by exactly one canonical SHA-256 literal.
///
/// The accepted window is an exactly quoted, 64-byte lowercase hexadecimal
/// string at the same byte offset in both inputs.  Bytes outside that window
/// must be identical.  Enumerating literal windows instead of counting changed
/// nibbles matters because two distinct hashes may share positions.
pub fn single_hash_literal_swap(before: &str, after: &str) -> Option<(String, String)> {
    let before = before.as_bytes();
    let after = after.as_bytes();
    if before.len() != after.len() || before == after || before.len() < 66 {
        return None;
    }

    let mut accepted = None;
    for start in 1..=before.len().saturating_sub(65) {
        let end = start + 64;
        if before[start - 1] != b'"'
            || before.get(end) != Some(&b'"')
            || after[start - 1] != b'"'
            || after.get(end) != Some(&b'"')
        {
            continue;
        }
        let old = std::str::from_utf8(&before[start..end]).ok()?;
        let new = std::str::from_utf8(&after[start..end]).ok()?;
        if !is_canonical_sha256(old)
            || !is_canonical_sha256(new)
            || old == new
            || before[..start] != after[..start]
            || before[end..] != after[end..]
        {
            continue;
        }
        if accepted.is_some() {
            return None;
        }
        accepted = Some((old.to_string(), new.to_string()));
    }
    accepted
}

/// Check the four authorization anchors for a frozen-contract supersession.
///
/// This is deliberately a pure authorization primitive.  The caller remains
/// responsible for sourcing the actor/TaskRecorded/original relocation facts
/// from the durable ledger and for emitting any lifecycle event.
pub fn frozen_contract_supersession_authorized(
    actor: &str,
    superseding_task_recorded: bool,
    original_seed_relocated_sha256: Option<&str>,
    declared_old_sha256: &str,
    current_landed_sha256: &str,
) -> std::result::Result<(), String> {
    if actor != "runtime:orch" {
        return Err("frozen-contract supersession 只允许 actor=runtime:orch".to_string());
    }
    if !superseding_task_recorded {
        return Err("替代合同尚无 TaskRecorded，拒绝 supersession".to_string());
    }
    frozen_contract_anchor_digests_match(
        original_seed_relocated_sha256,
        declared_old_sha256,
        current_landed_sha256,
    )
}

fn frozen_contract_anchor_digests_match(
    original_seed_relocated_sha256: Option<&str>,
    declared_old_sha256: &str,
    current_landed_sha256: &str,
) -> std::result::Result<(), String> {
    if !is_canonical_sha256(declared_old_sha256) || !is_canonical_sha256(current_landed_sha256) {
        return Err("supersession 摘要必须是规范的小写 SHA-256".to_string());
    }
    let original = original_seed_relocated_sha256
        .ok_or_else(|| "缺少原始 SeedRelocated.sha256 锚点".to_string())?;
    if !is_canonical_sha256(original) {
        return Err("原始 SeedRelocated.sha256 不是规范的小写 SHA-256".to_string());
    }
    if declared_old_sha256 != current_landed_sha256 {
        return Err("declared old 摘要与当前落位字节不一致".to_string());
    }
    if declared_old_sha256 != original {
        return Err("declared old 摘要与原始 SeedRelocated.sha256 不一致".to_string());
    }
    Ok(())
}

fn current_landed_digest(canonical_root: &Path, target: &str) -> Result<Option<String>> {
    let relative = canonical_repo_relative(target, "landed target")?;
    inspect_target_path(canonical_root, &relative, target)?;
    match fs::symlink_metadata(canonical_root.join(&relative)) {
        Ok(_) => {
            let (bytes, _) = read_checked_seed(canonical_root, &relative, target)?;
            Ok(Some(sha256_hex(&bytes)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("读取 landed seed target 失败: {target}")),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineManifest {
    schema_version: u32,
    baseline_tree_sha: String,
    scope: BaselineScope,
    counts: BaselineCounts,
    excluded_unrecorded_missing_targets: Vec<String>,
    targets: Vec<BaselineTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineScope {
    declared_pair_audit_through: String,
    effective_baseline_through: String,
    selection: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineCounts {
    declared_pairs_through_r70: usize,
    drifted_declared_pairs_through_r70: usize,
    missing_declared_pairs_through_r70: usize,
    unique_effective_targets_through_b269: usize,
    present_effective_targets: usize,
    effective_tombstones: usize,
    excluded_unrecorded_missing_targets: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineTarget {
    target: String,
    state: String,
    effective_sha256: Option<String>,
    effective_anchor: BaselineAnchor,
    grandfathered_drift: bool,
    sources: Vec<BaselineSource>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineAnchor {
    kind: String,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    baseline_tree_sha: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineSource {
    round: String,
    task_id: String,
    card_path: String,
    seed_src: String,
    declared_sha256: String,
    source_sha256: String,
    source_matches_declared: bool,
    drifted_from_source: bool,
    seed_relocated: Vec<BaselineRelocation>,
    task_recorded_event_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaselineRelocation {
    event_id: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SeedRelocatedPayload {
    cmp: String,
    sha256: String,
    target: String,
    #[serde(rename = "freezePolicy", default)]
    freeze_policy: SeedFreezePolicy,
    #[serde(default)]
    ir_revision: Option<u32>,
    #[serde(default)]
    seed_oracle_event_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SeedFreezePolicy {
    #[default]
    Permanent,
    Evolvable,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct V3SeedOraclePayload {
    oracle_schema_version: u32,
    expected_red: String,
    expected_red_proof: serde_json::Value,
    seeds: Vec<V3SeedOracleEntry>,
    ir_revision: u32,
    measured: serde_json::Value,
    #[serde(default)]
    supersedes_seed_oracle_event_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct V3SeedOracleEntry {
    target: String,
    sha256: String,
}

fn relocation_round_is_schema3(events: &[EventRecord], round: &str) -> Result<bool> {
    let opened = events
        .iter()
        .filter(|event| event.kind == "RoundOpened" && event.round.as_deref() == Some(round))
        .collect::<Vec<_>>();
    if opened.len() > 1 {
        bail!("SeedRelocated round {round} 的 RoundOpened 重复");
    }
    let Some(opened) = opened.first() else {
        return Ok(false);
    };
    if opened.actor != "runtime:orch" || opened.task_id.is_some() {
        bail!("SeedRelocated round {round} 的 RoundOpened 非唯一 canonical event");
    }
    let marker = opened
        .payload
        .as_ref()
        .and_then(|payload| payload.get("contractSchemaVersion"));
    match marker {
        None => Ok(false),
        Some(value) if value.as_u64() == Some(3) => Ok(true),
        Some(_) => bail!("SeedRelocated round {round} contractSchemaVersion 未建模"),
    }
}

fn decode_v3_seed_oracle(
    event: &EventRecord,
    round: &str,
    task_id: &str,
) -> Result<V3SeedOraclePayload> {
    let payload: V3SeedOraclePayload = serde_json::from_value(
        event
            .payload
            .clone()
            .context("schema 3 SeedOracleVerified 缺 payload")?,
    )
    .context("schema 3 SeedOracleVerified payload 非 closed schema")?;
    if event.kind != "SeedOracleVerified"
        || event.actor != "planner"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(task_id)
        || payload.oracle_schema_version != 2
        || payload.ir_revision == 0
        || payload.expected_red.trim().is_empty()
        || payload.expected_red_proof.is_null()
        || payload.measured.is_null()
        || payload
            .supersedes_seed_oracle_event_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.trim() != value)
    {
        bail!("schema 3 SeedOracleVerified envelope/payload 非 canonical");
    }
    Ok(payload)
}

fn v3_seed_map(payload: &V3SeedOraclePayload) -> Result<BTreeMap<String, String>> {
    let mut seeds = BTreeMap::new();
    for seed in &payload.seeds {
        canonical_repo_relative(&seed.target, "schema 3 seed oracle target")?;
        if !is_canonical_sha256(&seed.sha256)
            || seeds
                .insert(seed.target.clone(), seed.sha256.clone())
                .is_some()
        {
            bail!("schema 3 SeedOracleVerified seeds 非 canonical/唯一");
        }
    }
    Ok(seeds)
}

fn validation_for_revision(
    events: &[EventRecord],
    round: &str,
    revision: u32,
) -> Result<(usize, crate::plan::TaskValidatedPayload)> {
    let matches = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "TaskValidated" && event.round.as_deref() == Some(round)
        })
        .map(|(position, event)| {
            crate::plan::decode_runtime_task_validated(event, round)
                .map(|payload| (position, payload))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, payload)| payload.ir_revision == revision)
        .collect::<Vec<_>>();
    let [(position, payload)] = matches.as_slice() else {
        bail!(
            "schema 3 revision {revision} 要求唯一 production TaskValidated，实得 {}",
            matches.len()
        );
    };
    Ok((*position, payload.clone()))
}

fn effective_v3_seed_oracle(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    signed_revision: u32,
    signed_digest: &str,
) -> Result<Option<(usize, String, V3SeedOraclePayload, BTreeMap<String, String>)>> {
    let (_, signed_validation) = validation_for_revision(events, round, signed_revision)?;
    if signed_validation.validation_digest != signed_digest {
        bail!("schema 3 relocation root revision/digest 未绑定同一 TaskValidated");
    }
    let signed_signoff = crate::plan::matching_user_plan_signoff_position(
        events,
        round,
        signed_revision,
        signed_digest,
    )?
    .context("schema 3 relocation history 缺 matching PlanSignedOff")?;
    let validations = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "TaskValidated" && event.round.as_deref() == Some(round)
        })
        .map(|(position, event)| {
            crate::plan::decode_runtime_task_validated(event, round)
                .map(|payload| (position, payload))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut candidates = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "SeedOracleVerified"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
    }) {
        let payload = decode_v3_seed_oracle(event, round, task_id)?;
        let (validation_position, _validation) =
            validation_for_revision(events, round, payload.ir_revision)?;
        if validation_position >= position {
            bail!("schema 3 SeedOracleVerified 必须晚于其 TaskValidated");
        }
        let seeds = v3_seed_map(&payload)?;
        if payload.ir_revision > signed_revision {
            continue;
        }
        let invalidated = validations.iter().any(|(_, later)| {
            later.ir_revision > payload.ir_revision
                && later.ir_revision <= signed_revision
                && later.reverify_tasks.iter().any(|candidate| candidate == task_id)
        });
        if !invalidated && position < signed_signoff {
            candidates.push((position, event.event_id.clone(), payload, seeds));
        }
    }

    let Some(highest_revision) = candidates
        .iter()
        .map(|(_, _, payload, _)| payload.ir_revision)
        .max()
    else {
        return Ok(None);
    };
    let selected = candidates
        .into_iter()
        .filter(|(_, _, payload, _)| payload.ir_revision == highest_revision)
        .collect::<Vec<_>>();
    let [selected] = selected.as_slice() else {
        bail!(
            "schema 3 task {task_id} effective seed oracle 非唯一，revision={highest_revision} count={}",
            selected.len()
        );
    };
    Ok(Some(selected.clone()))
}

/// Build missing schema-3 relocation facts for one signed seed epoch.
/// Existing identical coordinates are reused; conflicts fail closed.
pub(crate) fn v3_seed_relocation_batch(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    seed_digests: &[(String, String)],
) -> Result<Vec<EventRecord>> {
    let validations = events
        .iter()
        .filter(|event| {
            event.kind == "TaskValidated" && event.round.as_deref() == Some(round)
        })
        .map(|event| crate::plan::decode_runtime_task_validated(event, round))
        .collect::<Result<Vec<_>>>()?;
    let signed = validations
        .last()
        .context("schema 3 collect 缺 production TaskValidated")?;
    let (_, oracle_id, _, expected) = effective_v3_seed_oracle(
        events,
        round,
        task_id,
        signed.ir_revision,
        &signed.validation_digest,
    )?
    .context("schema 3 seeded collect 缺 effective SeedOracleVerified")?;

    let mut measured = BTreeMap::new();
    for (target, digest) in seed_digests {
        canonical_repo_relative(target, "schema 3 collected seed target")?;
        if !is_canonical_sha256(digest)
            || measured.insert(target.clone(), digest.clone()).is_some()
        {
            bail!("schema 3 collected seed set 非 canonical/唯一");
        }
    }
    if measured != expected {
        bail!("schema 3 collected seed set 与 effective SeedOracleVerified 不一致");
    }

    let mut existing = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.kind == "SeedRelocated"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
    }) {
        let payload: SeedRelocatedPayload = serde_json::from_value(
            event.payload.clone().context("schema 3 SeedRelocated 缺 payload")?,
        )
        .context("schema 3 SeedRelocated payload 非 closed schema")?;
        if payload.ir_revision == Some(signed.ir_revision)
            && payload.seed_oracle_event_id.as_deref() == Some(oracle_id.as_str())
        {
            if payload.cmp != "identical"
                || payload.freeze_policy != SeedFreezePolicy::Evolvable
                || !is_canonical_sha256(&payload.sha256)
                || existing
                    .insert(payload.target.clone(), payload.sha256.clone())
                    .is_some()
            {
                bail!("schema 3 collect 已有冲突/重复 SeedRelocated coordinate");
            }
        } else if payload.ir_revision == Some(signed.ir_revision) {
            bail!("schema 3 collect 同 revision 已绑定不同 seed oracle");
        }
    }
    let mut batch = Vec::new();
    for (target, digest) in measured {
        match existing.get(&target) {
            Some(prior) if prior == &digest => continue,
            Some(_) => bail!("schema 3 collect 同 epoch seed digest 冲突: {target}"),
            None => {}
        }
        batch.push(ledger::event(
            "SeedRelocated",
            "runtime:orch",
            Some(task_id),
            Some(round),
            serde_json::json!({
                "target": target,
                "sha256": digest,
                "cmp": "identical",
                "freezePolicy": "evolvable",
                "irRevision": signed.ir_revision,
                "seedOracleEventId": oracle_id,
            }),
        ));
    }
    Ok(batch)
}

/// Validate schema-3 `SeedRelocated` facts without turning them into permanent
/// landed anchors. `require_recorded` is used by archived replay; live verdict
/// validation leaves the future `TaskRecorded` check to the record boundary.
pub fn validate_v3_relocation_history(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    signed_revision: u32,
    signed_digest: &str,
    require_recorded: bool,
    expected_has_seeds: bool,
) -> Result<()> {
    if !relocation_round_is_schema3(events, round)? {
        bail!("v3 relocation validator 要求 schema 3 RoundOpened");
    }
    let current_oracle = effective_v3_seed_oracle(
        events,
        round,
        task_id,
        signed_revision,
        signed_digest,
    )?;
    let relocations = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "SeedRelocated"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
        })
        .collect::<Vec<_>>();
    if !expected_has_seeds {
        if current_oracle.is_some()
            || relocations.iter().any(|(_, event)| {
                event.payload.as_ref().is_some_and(|payload| {
                    payload.get("irRevision").and_then(serde_json::Value::as_u64)
                        == Some(u64::from(signed_revision))
                })
            })
        {
            bail!("verify-only schema 3 task 当前 revision 不得含 effective seed facts");
        }
        return Ok(());
    }
    let (_, oracle_event_id, _, expected) = current_oracle
        .context("seeded schema 3 task 缺 effective SeedOracleVerified")?;
    if expected.is_empty() {
        bail!("seeded schema 3 task 的 SeedOracleVerified seeds 不得为空");
    }
    let mut observed = BTreeMap::new();
    for (position, event) in relocations {
        let payload: SeedRelocatedPayload = serde_json::from_value(
            event
                .payload
                .clone()
                .context("schema 3 SeedRelocated 缺 payload")?,
        )
        .context("schema 3 SeedRelocated payload 非 closed schema")?;
        if event.actor != "runtime:orch"
            || payload.cmp != "identical"
            || payload.freeze_policy != SeedFreezePolicy::Evolvable
            || !is_canonical_sha256(&payload.sha256)
            || payload.ir_revision.is_none()
            || payload
                .seed_oracle_event_id
                .as_deref()
                .is_none_or(str::is_empty)
        {
            bail!("schema 3 SeedRelocated envelope/freezePolicy 非 canonical");
        }
        canonical_repo_relative(&payload.target, "schema 3 relocated target")?;
        let relocation_revision = payload.ir_revision.context("schema 3 relocation 缺 revision")?;
        let (_, relocation_validation) =
            validation_for_revision(events, round, relocation_revision)?;
        let (oracle_position, effective_id, _, effective_seeds) = effective_v3_seed_oracle(
            events,
            round,
            task_id,
            relocation_revision,
            &relocation_validation.validation_digest,
        )?
        .context("schema 3 relocation 缺 effective oracle")?;
        if position <= oracle_position
            || payload.seed_oracle_event_id.as_deref() != Some(effective_id.as_str())
            || effective_seeds.get(&payload.target) != Some(&payload.sha256)
        {
            bail!("schema 3 SeedRelocated oracle epoch/provenance 不匹配");
        }
        let selected_epoch = relocation_revision == signed_revision && effective_id == oracle_event_id;
        if selected_epoch
            && observed
                .insert(payload.target.clone(), payload.sha256.clone())
                .is_some()
        {
            bail!("schema 3 current seed epoch target 重复");
        }
        if require_recorded
            && selected_epoch
            && !events[position + 1..].iter().any(|candidate| {
                candidate.kind == "TaskRecorded"
                    && candidate.actor == "runtime:orch"
                    && candidate.round.as_deref() == Some(round)
                    && candidate.task_id.as_deref() == Some(task_id)
                    && candidate
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("postMergeGates"))
                        .and_then(serde_json::Value::as_str)
                        == Some("all-green")
            })
        {
            bail!("schema 3 SeedRelocated 缺后继 all-green TaskRecorded");
        }
    }
    if observed != expected {
        bail!("schema 3 SeedRelocated 必须精确覆盖 current SeedOracleVerified seed set");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LandedAnchor {
    digest: Option<String>,
    effective: card::FrozenEffectiveAnchor,
    original_event_id: String,
    original_sha256: String,
    grandfathered_drift: bool,
}

fn round_ledger_sort_key(path: &str) -> (u64, &str) {
    let round = path
        .strip_prefix("coordination/rounds/r")
        .and_then(|tail| tail.split('/').next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    (round, path)
}

/// Read ledger events from one immutable tree, preserving line order inside
/// every ledger.  Cross-ledger order is numeric round order, never timestamp
/// or event-id order (both are presentation data, not canonical sequencing).
fn committed_tree_events(root: &Path, tree_oid: &str) -> Result<Vec<EventRecord>> {
    if !is_full_commit_oid(tree_oid) {
        bail!("ledger census 需要完整 commit OID: {tree_oid:?}");
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-tree",
            "-r",
            "--full-tree",
            "--name-only",
            "-z",
            tree_oid,
            "--",
            "coordination/rounds",
        ])
        .output()
        .context("git ls-tree landed-seed ledgers 启动失败")?;
    if !output.status.success() {
        bail!(
            "git ls-tree landed-seed ledgers 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| std::str::from_utf8(bytes).map(str::to_owned))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("ledger tree 含非 UTF-8 路径")?;
    paths.retain(|path| path.ends_with("/events.jsonl"));
    paths.sort_by(|left, right| round_ledger_sort_key(left).cmp(&round_ledger_sort_key(right)));

    let mut events = Vec::new();
    for path in paths {
        let bytes = gitx::show_bytes(root, tree_oid, &path)?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("committed ledger 非 UTF-8: {path}"))?;
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            events.push(
                serde_json::from_str::<EventRecord>(line)
                    .with_context(|| format!("committed ledger 坏行: {path}:{}", index + 1))?,
            );
        }
    }
    Ok(events)
}

fn event_by_id<'a>(events: &'a [EventRecord], event_id: &str) -> Result<&'a EventRecord> {
    let mut matches = events.iter().filter(|event| event.event_id == event_id);
    let event = matches
        .next()
        .with_context(|| format!("landed baseline 引用不存在的 eventId: {event_id}"))?;
    if matches.next().is_some() {
        bail!("landed baseline eventId 非全局唯一: {event_id}");
    }
    Ok(event)
}

fn unique_event_position(events: &[EventRecord], event_id: &str) -> Result<usize> {
    let positions = events
        .iter()
        .enumerate()
        .filter_map(|(position, event)| (event.event_id == event_id).then_some(position))
        .collect::<Vec<_>>();
    match positions.as_slice() {
        [position] => Ok(*position),
        [] => bail!("landed baseline 引用不存在的 eventId: {event_id}"),
        _ => bail!("landed baseline eventId 非全局唯一: {event_id}"),
    }
}

fn seed_oracle_precedes(
    events: &[EventRecord],
    position: usize,
    round: &str,
    task_id: &str,
    target: &str,
    digest: &str,
) -> bool {
    events[..position].iter().rev().any(|event| {
        event.kind == "SeedOracleVerified"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("seeds"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|seeds| {
                    seeds.iter().any(|seed| {
                        seed.get("target").and_then(serde_json::Value::as_str) == Some(target)
                            && seed.get("sha256").and_then(serde_json::Value::as_str)
                                == Some(digest)
                    })
                })
    })
}

/// Reconstruct the active, user-signed ROUND-IR from one immutable main tree.
///
/// This deliberately does not use `require_active_round_ir`: that helper reads
/// live worktree files and would recurse through seed validation.  The landed
/// guard must bind the exact main object captured by `mech::check` instead.
fn exact_main_signed_ir_with_binding_policy(
    root: &Path,
    main_oid: &str,
    binding_bytes: &[u8],
    events: &[EventRecord],
    require_current_binding_match: bool,
) -> Result<(String, crate::plan::RoundIr)> {
    // CURRENT-ROUND is runtime state and intentionally not committed.  In an
    // immutable tree, the highest numeric ROUND-IR is the durable active-round
    // projection; its production validation/sign-off below is the authority.
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-tree",
            "-r",
            "--full-tree",
            "--name-only",
            main_oid,
            "--",
            "coordination/rounds",
        ])
        .output()
        .context("git ls-tree exact-main ROUND-IR 启动失败")?;
    if !output.status.success() {
        bail!(
            "git ls-tree exact-main ROUND-IR 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let round = String::from_utf8(output.stdout)
        .context("exact-main ROUND-IR 路径非 UTF-8")?
        .lines()
        .filter_map(|path| {
            path.strip_prefix("coordination/rounds/r")
                .and_then(|tail| tail.strip_suffix("/ROUND-IR.yaml"))
                .and_then(|number| number.parse::<u64>().ok())
                .map(|number| (number, format!("r{number}")))
        })
        .max_by_key(|(number, _)| *number)
        .map(|(_, round)| round)
        .context("精确 main 缺 signed ROUND-IR")?;
    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let ir_bytes = gitx::show_bytes(root, main_oid, &ir_rel)
        .with_context(|| format!("从精确 main 读取 signed ROUND-IR 失败: {ir_rel}"))?;
    let ir_text = std::str::from_utf8(&ir_bytes).context("精确 main ROUND-IR 非 UTF-8")?;
    let ir = crate::plan::parse_signed_round_ir(ir_text).map_err(anyhow::Error::msg)?;
    if ir.round != round {
        bail!(
            "精确 main ROUND-IR round={} 与 CURRENT-ROUND {round} 不一致",
            ir.round
        );
    }
    let digest = crate::plan::validation_digest(&ir);

    let mut high_water: Option<(u32, String)> = None;
    for event in events.iter().filter(|event| {
        event.kind == "TaskValidated"
            && event.actor == "runtime:orch"
            && event.task_id.is_none()
            && event.round.as_deref() == Some(round.as_str())
    }) {
        let payload = crate::plan::decode_runtime_task_validated(event, &round)?;
        if let Some((revision, prior_digest)) = &high_water {
            if payload.ir_revision < *revision
                || (payload.ir_revision == *revision && payload.validation_digest != *prior_digest)
            {
                bail!("精确 main TaskValidated high-water 非单调或同 revision 分叉");
            }
        }
        high_water = Some((payload.ir_revision, payload.validation_digest));
    }
    if high_water.as_ref() != Some(&(ir.revision, digest.clone())) {
        bail!(
            "精确 main ROUND-IR 不是最高 production TaskValidated: ir=({}, {}) high={high_water:?}",
            ir.revision,
            digest
        );
    }
    if crate::plan::matching_user_plan_signoff_position(events, &round, ir.revision, &digest)?
        .is_none()
    {
        bail!("精确 main ROUND-IR 缺 matching user PlanSignedOff");
    }
    if require_current_binding_match
        && ir.source_bindings.binding_sha256 != sha256_hex(binding_bytes)
    {
        bail!("精确 main PROJECT-BINDING bytes 未绑定 signed ROUND-IR");
    }
    Ok((round, ir))
}

fn exact_main_signed_ir(
    root: &Path,
    main_oid: &str,
    binding_bytes: &[u8],
    events: &[EventRecord],
) -> Result<(String, crate::plan::RoundIr)> {
    exact_main_signed_ir_with_binding_policy(root, main_oid, binding_bytes, events, true)
}

/// Find the ancestor version of one path whose complete bytes match a digest
/// already carried by a signed ROUND-IR.  This is used only while authorizing
/// the *next* plan: unrelated planner-owned binding edits are not signed yet,
/// but the immutable landed-seed descriptor still has to come from signed
/// history rather than from the mutable worktree.
fn ancestor_path_bytes_by_sha256(
    root: &Path,
    main_oid: &str,
    path: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>> {
    if !is_canonical_sha256(expected_sha256) {
        bail!("signed historical path digest 非 canonical SHA-256");
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", main_oid, "--", path])
        .output()
        .context("git rev-list historical binding 启动失败")?;
    if !output.status.success() {
        bail!(
            "git rev-list historical binding 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    for commit in String::from_utf8(output.stdout)
        .context("historical binding rev-list 非 UTF-8")?
        .lines()
    {
        let bytes = gitx::show_bytes(root, commit, path)
            .with_context(|| format!("读取 historical binding 失败: {commit}:{path}"))?;
        if sha256_hex(&bytes) == expected_sha256 {
            return Ok(bytes);
        }
    }
    bail!("找不到 signed ROUND-IR 绑定的 historical PROJECT-BINDING bytes")
}

fn validate_pending_binding_baseline_descriptor(
    current: &binding::LandedSeedBaselineDescriptor,
    signed_binding_bytes: &[u8],
) -> Result<()> {
    let signed = binding::parse_binding_bytes(signed_binding_bytes).map_err(anyhow::Error::msg)?;
    if signed.oracle.landed_seed_baseline.as_ref() != Some(current) {
        bail!("首次 plan 前 landedSeedBaseline descriptor 漂移；必须先由既有签名授权")
    }
    Ok(())
}

fn round_ordinal(round: &str) -> Option<u64> {
    let number = round.strip_prefix('r')?;
    if number.is_empty()
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    number.parse().ok()
}

/// The only unsigned-binding window admitted by landed census is the first
/// plan of the immediately following round: `RoundOpened` is durable, but no
/// `TaskValidated` exists yet because this very command is producing it.
fn validate_first_plan_pending_round(events: &[EventRecord], signed_round: &str) -> Result<String> {
    let signed = round_ordinal(signed_round).context("signed ROUND-IR round 非 canonical")?;
    let mut pending = BTreeSet::new();
    for event in events {
        let Some(round) = event.round.as_deref() else {
            continue;
        };
        let Some(number) = round_ordinal(round) else {
            continue;
        };
        if number <= signed {
            continue;
        }
        if event.kind == "RoundOpened" && event.actor == "runtime:orch" && event.task_id.is_none() {
            pending.insert(round.to_string());
        }
    }
    let expected = format!("r{}", signed + 1);
    if pending.len() != 1 || !pending.contains(&expected) {
        bail!(
            "精确 main PROJECT-BINDING bytes 未绑定 signed ROUND-IR；unsigned binding 只允许紧邻 signed round 的首次 plan 窗口"
        )
    }
    if events.iter().any(|event| {
        event.round.as_deref() == Some(expected.as_str()) && event.kind == "TaskValidated"
    }) {
        bail!(
            "精确 main PROJECT-BINDING bytes 未绑定 signed ROUND-IR；pending round 已有 TaskValidated"
        )
    }
    Ok(expected)
}

#[cfg(test)]
mod pending_binding_baseline_tests {
    use super::*;

    fn descriptor() -> binding::LandedSeedBaselineDescriptor {
        binding::LandedSeedBaselineDescriptor {
            schema_version: 1,
            path: "coordination/frozen-contract-baseline-v1.json".to_string(),
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn unrelated_pending_binding_fields_keep_the_signed_baseline_authority() {
        let bytes = br#"
oracle:
  landedSeedBaseline:
    schemaVersion: 1
    path: coordination/frozen-contract-baseline-v1.json
    sha256: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
runtimePolicies:
  ignoredByOldBinary: true
"#;
        validate_pending_binding_baseline_descriptor(&descriptor(), bytes).unwrap();
    }

    #[test]
    fn a_pending_baseline_descriptor_change_is_still_rejected() {
        let bytes = br#"
oracle:
  landedSeedBaseline:
    schemaVersion: 1
    path: coordination/other.json
    sha256: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
"#;
        assert!(validate_pending_binding_baseline_descriptor(&descriptor(), bytes).is_err());
    }

    #[test]
    fn unsigned_binding_is_only_admitted_during_the_next_rounds_first_plan() {
        let opened = ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r81"),
            serde_json::json!({"purpose": "fixture"}),
        );
        assert_eq!(
            validate_first_plan_pending_round(&[opened.clone()], "r80").unwrap(),
            "r81"
        );
        assert!(validate_first_plan_pending_round(&[], "r80").is_err());
        let validated = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r81"),
            serde_json::json!({"irRevision": 1, "validationDigest": "a".repeat(64)}),
        );
        assert!(validate_first_plan_pending_round(&[opened, validated], "r80").is_err());
    }
}

/// Load the exact-main task card whenever a caller presents a supersession.
/// Returning the parsed committed card lets the whole mechanical check consume
/// signed write-set/seed/gate data instead of trusting a caller-built `Card`.
pub(crate) fn load_exact_main_signed_card(
    root: &Path,
    supplied: &card::Card,
    main_oid: &str,
) -> Result<Option<card::Card>> {
    if supplied.meta.frozen_contract_supersessions.is_empty() {
        return Ok(None);
    }
    let round = supplied
        .meta
        .round
        .as_deref()
        .context("supersession card 缺 round")?;
    let task_id = supplied.meta.task_id.as_str();
    card::validate_task_id(task_id)?;
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    if supplied.rel_path != card_rel {
        bail!(
            "supersession caller card path 未绑定 canonical task path: {}",
            supplied.rel_path
        );
    }

    let binding_rel = "coordination/PROJECT-BINDING.yaml";
    let binding_bytes = gitx::show_bytes(root, main_oid, binding_rel)
        .context("从精确 main 读取 PROJECT-BINDING 失败")?;
    binding::parse_binding_bytes(&binding_bytes).map_err(anyhow::Error::msg)?;
    let events = committed_tree_events(root, main_oid)?;
    let (active_round, ir) = exact_main_signed_ir(root, main_oid, &binding_bytes, &events)?;
    if active_round != round {
        bail!("supersession card round={round} 不是 active signed round={active_round}");
    }
    if ir.tasks.iter().filter(|task| task.id == task_id).count() != 1 {
        bail!("signed ROUND-IR 必须恰好包含一次 supersession task: {task_id}");
    }
    let expected = ir
        .source_bindings
        .task_cards
        .get(&card_rel)
        .with_context(|| format!("signed ROUND-IR 未绑定 supersession card: {card_rel}"))?;
    let card_bytes = gitx::show_bytes(root, main_oid, &card_rel)
        .context("从精确 main 读取 supersession task card 失败")?;
    if sha256_hex(&card_bytes) != *expected {
        bail!("精确 main supersession card bytes 未绑定 signed ROUND-IR");
    }
    let card_text = std::str::from_utf8(&card_bytes).context("supersession card 非 UTF-8")?;
    let exact = card::parse(&card_rel, task_id, card_text)?;
    if exact.meta.round.as_deref() != Some(round)
        || exact.meta.frozen_contract_supersessions != supplied.meta.frozen_contract_supersessions
    {
        bail!("caller supersession declaration 与 exact signed card 不一致");
    }
    Ok(Some(exact))
}

fn payload_string<'a>(event: &'a EventRecord, key: &str) -> Result<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("{}({}) 缺 payload.{key}", event.kind, event.event_id))
}

fn validate_relocation_event(
    events: &[EventRecord],
    event_id: &str,
    round: &str,
    task_id: &str,
    target: &str,
    digest: &str,
) -> Result<()> {
    let position = unique_event_position(events, event_id)?;
    let event = &events[position];
    let payload: SeedRelocatedPayload =
        serde_json::from_value(event.payload.clone().context("SeedRelocated 缺 payload")?)
            .context("SeedRelocated payload 非 closed schema")?;
    if event.kind != "SeedRelocated"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(task_id)
        || payload.cmp != "identical"
        || payload.target != target
        || payload.sha256 != digest
        || !is_canonical_sha256(&payload.sha256)
        || !seed_oracle_precedes(events, position, round, task_id, target, digest)
    {
        bail!("SeedRelocated provenance 不匹配: {event_id}");
    }
    Ok(())
}

fn validate_recorded_event(
    events: &[EventRecord],
    event_id: &str,
    round: &str,
    task_id: &str,
) -> Result<()> {
    let event = event_by_id(events, event_id)?;
    if event.kind != "TaskRecorded"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(task_id)
        || payload_string(event, "postMergeGates")? != "all-green"
    {
        bail!("TaskRecorded provenance 不匹配或非 all-green: {event_id}");
    }
    Ok(())
}

fn baseline_anchor_as_card(anchor: &BaselineAnchor) -> card::FrozenEffectiveAnchor {
    card::FrozenEffectiveAnchor {
        kind: anchor.kind.clone(),
        event_id: anchor.event_id.clone(),
        baseline_tree_sha: anchor.baseline_tree_sha.clone(),
        sha256: anchor.sha256.clone(),
    }
}

fn load_signed_baseline(
    root: &Path,
    main_oid: &str,
) -> Result<(BTreeMap<String, LandedAnchor>, Vec<EventRecord>)> {
    if !is_full_commit_oid(main_oid) {
        bail!("landed baseline 需要完整 main commit OID: {main_oid:?}");
    }
    let binding_path = "coordination/PROJECT-BINDING.yaml";
    if !gitx::tree_path_exists(root, main_oid, binding_path)? {
        return Ok((BTreeMap::new(), Vec::new()));
    }
    let binding_bytes = gitx::show_bytes(root, main_oid, binding_path)?;
    let bound = binding::parse_binding_bytes(&binding_bytes).map_err(anyhow::Error::msg)?;
    let events = committed_tree_events(root, main_oid)?;
    let fallback_descriptor;
    let descriptor = if let Some(descriptor) = bound.oracle.landed_seed_baseline.as_ref() {
        let (_, signed_ir) =
            exact_main_signed_ir_with_binding_policy(root, main_oid, &binding_bytes, &events, false)
                .context("landed baseline descriptor 未被 signed round 覆盖")?;
        if signed_ir.source_bindings.binding_sha256 != sha256_hex(&binding_bytes) {
            validate_first_plan_pending_round(&events, &signed_ir.round)?;
            let signed_binding_bytes = ancestor_path_bytes_by_sha256(
                root,
                main_oid,
                binding_path,
                &signed_ir.source_bindings.binding_sha256,
            )?;
            validate_pending_binding_baseline_descriptor(descriptor, &signed_binding_bytes)?;
        }
        descriptor
    } else {
        match authenticated_closed_legacy_baseline_descriptor(root, main_oid, &events)? {
            LegacyBaselineRecovery::Declared(recovered) => {
                fallback_descriptor = recovered;
                &fallback_descriptor
            }
            LegacyBaselineRecovery::AuthenticatedlyAbsent
            | LegacyBaselineRecovery::NoLegacyHistory => {
                return Ok((BTreeMap::new(), events));
            }
        }
    };
    if descriptor.schema_version != 1
        || !is_canonical_sha256(&descriptor.sha256)
        || canonical_repo_relative(&descriptor.path, "landed baseline path")?.to_string_lossy()
            != descriptor.path
    {
        bail!("oracle.landedSeedBaseline descriptor 非规范");
    }
    let manifest_bytes = gitx::show_bytes(root, main_oid, &descriptor.path)
        .context("从精确 main 读取 landed baseline manifest 失败")?;
    // The binding module owns the closed project-data schema.  The local
    // projection below adds ledger provenance and effective-delta checks.
    binding::parse_landed_seed_baseline(descriptor, &manifest_bytes).map_err(anyhow::Error::msg)?;
    let actual_manifest_sha = sha256_hex(&manifest_bytes);
    if actual_manifest_sha != descriptor.sha256 {
        bail!(
            "landed baseline manifest digest 不匹配: descriptor={} actual={}",
            descriptor.sha256,
            actual_manifest_sha
        );
    }
    let manifest: BaselineManifest =
        serde_json::from_slice(&manifest_bytes).context("landed baseline manifest 严格解析失败")?;
    let declared_through = manifest
        .scope
        .declared_pair_audit_through
        .strip_prefix('r')
        .and_then(|value| value.parse::<u64>().ok())
        .context("landed baseline declaredPairAuditThrough 非 canonical round")?;
    let effective_through = manifest
        .scope
        .effective_baseline_through
        .split_once('/')
        .context("landed baseline effectiveBaselineThrough 非 rN/taskId")?;
    if manifest.schema_version != 1
        || !is_full_commit_oid(&manifest.baseline_tree_sha)
        || effective_through
            .0
            .strip_prefix('r')
            .and_then(|value| value.parse::<u64>().ok())
            .is_none()
        || card::validate_task_id(effective_through.1).is_err()
        || manifest.scope.selection
            != "targets with a durable SeedRelocated fact; later SeedRelocated/FrozenContractSuperseded facts are deltas"
    {
        bail!("landed baseline manifest schema/scope 非 canonical v1");
    }
    let mut exclusions = BTreeSet::new();
    let mut previous_exclusion: Option<&str> = None;
    for target in &manifest.excluded_unrecorded_missing_targets {
        canonical_repo_relative(target, "excluded landed target")?;
        if previous_exclusion.is_some_and(|previous| previous >= target.as_str())
            || !exclusions.insert(target.clone())
        {
            bail!("excluded landed targets 必须严格排序且唯一");
        }
        previous_exclusion = Some(target);
    }

    let mut anchors = BTreeMap::new();
    let mut previous_target: Option<&str> = None;
    let mut through_r70 = 0usize;
    let mut drifted_through_r70 = 0usize;
    let mut present = 0usize;
    let mut tombstones = 0usize;
    let mut genesis_relocation_ids = BTreeSet::new();
    for target in &manifest.targets {
        canonical_repo_relative(&target.target, "baseline target")?;
        if previous_target.is_some_and(|previous| previous >= target.target.as_str())
            || exclusions.contains(&target.target)
        {
            bail!("baseline targets 必须严格排序、唯一且不得命中 exclusions");
        }
        previous_target = Some(&target.target);
        if target.sources.is_empty() {
            bail!("baseline target 缺 provenance sources: {}", target.target);
        }
        let mut original: Option<(String, String)> = None;
        let mut source_drift = false;
        for source in &target.sources {
            canonical_repo_relative(&source.card_path, "baseline cardPath")?;
            canonical_repo_relative(&source.seed_src, "baseline seedSrc")?;
            if source.card_path
                != format!(
                    "coordination/rounds/{}/tasks/{}.md",
                    source.round, source.task_id
                )
                || !is_canonical_sha256(&source.declared_sha256)
                || !is_canonical_sha256(&source.source_sha256)
                || source.source_matches_declared
                    != (source.declared_sha256 == source.source_sha256)
            {
                bail!("baseline source binding 非规范: {}", target.target);
            }
            let number = source
                .round
                .strip_prefix('r')
                .and_then(|value| value.parse::<u64>().ok())
                .context("baseline source round 非 r<数字>")?;
            if number <= declared_through {
                through_r70 += 1;
                if source.drifted_from_source {
                    drifted_through_r70 += 1;
                }
            }
            source_drift |= source.drifted_from_source;
            for relocation in &source.seed_relocated {
                if !is_canonical_sha256(&relocation.sha256) {
                    bail!("baseline SeedRelocated sha256 非规范");
                }
                if !genesis_relocation_ids.insert(relocation.event_id.clone()) {
                    bail!(
                        "baseline SeedRelocated eventId 重复: {}",
                        relocation.event_id
                    );
                }
                validate_relocation_event(
                    &events,
                    &relocation.event_id,
                    &source.round,
                    &source.task_id,
                    &target.target,
                    &relocation.sha256,
                )?;
                original.get_or_insert_with(|| {
                    (relocation.event_id.clone(), relocation.sha256.clone())
                });
            }
            for event_id in &source.task_recorded_event_ids {
                validate_recorded_event(&events, event_id, &source.round, &source.task_id)?;
            }
        }
        let (original_event_id, original_sha256) = original.with_context(|| {
            format!(
                "baseline target 缺 target-level SeedRelocated: {}",
                target.target
            )
        })?;
        let effective = baseline_anchor_as_card(&target.effective_anchor);
        match target.effective_anchor.kind.as_str() {
            "seed-relocated" => {
                present += 1;
                if target.effective_anchor.event_id.is_none()
                    || target.effective_anchor.baseline_tree_sha.is_some()
                    || target.effective_anchor.sha256.as_deref()
                        != target.effective_sha256.as_deref()
                    || target.effective_sha256.as_deref() != Some(&original_sha256)
                    || target.grandfathered_drift
                    || source_drift
                {
                    bail!("seed-relocated effective anchor 非规范: {}", target.target);
                }
                let event_id = target.effective_anchor.event_id.as_deref().unwrap();
                let event = event_by_id(&events, event_id)?;
                if event.kind != "SeedRelocated"
                    || event.actor != "runtime:orch"
                    || payload_string(event, "target")? != target.target
                    || payload_string(event, "sha256")?
                        != target.effective_sha256.as_deref().unwrap()
                {
                    bail!("effective SeedRelocated anchor 不匹配: {event_id}");
                }
            }
            "migration-baseline" => {
                present += 1;
                if target.effective_anchor.event_id.is_some()
                    || target.effective_anchor.baseline_tree_sha.as_deref()
                        != Some(&manifest.baseline_tree_sha)
                    || target.effective_anchor.sha256.as_deref()
                        != target.effective_sha256.as_deref()
                    || target
                        .effective_sha256
                        .as_deref()
                        .is_none_or(|value| !is_canonical_sha256(value))
                    || !target.grandfathered_drift
                    || !source_drift
                {
                    bail!("migration-baseline anchor 非规范: {}", target.target);
                }
            }
            "migration-tombstone" => {
                tombstones += 1;
                if target.effective_anchor.event_id.is_some()
                    || target.effective_anchor.baseline_tree_sha.as_deref()
                        != Some(&manifest.baseline_tree_sha)
                    || target.effective_anchor.sha256.is_some()
                    || target.effective_sha256.is_some()
                    || target.grandfathered_drift
                {
                    bail!("migration-tombstone anchor 非规范: {}", target.target);
                }
            }
            other => bail!("baseline effectiveAnchor.kind 非闭枚举: {other}"),
        }
        if (target.state == "present") != target.effective_sha256.is_some()
            || (target.state == "tombstone") != target.effective_sha256.is_none()
        {
            bail!("baseline target state/digest 不一致: {}", target.target);
        }
        anchors.insert(
            target.target.clone(),
            LandedAnchor {
                digest: target.effective_sha256.clone(),
                effective,
                original_event_id,
                original_sha256,
                grandfathered_drift: target.grandfathered_drift,
            },
        );
    }
    let counts = &manifest.counts;
    if counts.declared_pairs_through_r70 != through_r70 + exclusions.len()
        || counts.drifted_declared_pairs_through_r70 != drifted_through_r70
        || counts.missing_declared_pairs_through_r70 != exclusions.len() + tombstones
        || counts.unique_effective_targets_through_b269 != anchors.len()
        || counts.present_effective_targets != present
        || counts.effective_tombstones != tombstones
        || counts.excluded_unrecorded_missing_targets != exclusions.len()
    {
        bail!("landed baseline manifest counts 不匹配");
    }

    // The manifest is immutable genesis.  Only durable, canonical events may
    // extend it: recorded later relocations add new targets; supersessions
    // advance one existing target's explicit effective-anchor chain.
    for (position, event) in events.iter().enumerate() {
        if event.kind == "SeedRelocated" {
            if genesis_relocation_ids.contains(&event.event_id) {
                continue;
            }
            let payload: SeedRelocatedPayload = serde_json::from_value(
                event
                    .payload
                    .clone()
                    .context("dynamic SeedRelocated 缺 payload")?,
            )
            .context("dynamic SeedRelocated payload 非 closed schema")?;
            let (Some(round), Some(task_id)) = (event.round.as_deref(), event.task_id.as_deref())
            else {
                bail!("dynamic SeedRelocated 缺 round/taskId: {}", event.event_id);
            };
            let schema3 = relocation_round_is_schema3(&events, round)?;
            match (schema3, payload.freeze_policy) {
                (true, SeedFreezePolicy::Evolvable)
                | (false, SeedFreezePolicy::Permanent) => {}
                (true, SeedFreezePolicy::Permanent) => {
                    bail!("schema 3 SeedRelocated 必须显式 freezePolicy=evolvable")
                }
                (false, SeedFreezePolicy::Evolvable) => {
                    bail!("legacy SeedRelocated 不得声明 evolvable")
                }
            }
            if payload.cmp != "identical"
                || !is_canonical_sha256(&payload.sha256)
                || event.actor != "runtime:orch"
            {
                bail!(
                    "dynamic SeedRelocated envelope/payload 非 canonical: {}",
                    event.event_id
                );
            }
            canonical_repo_relative(&payload.target, "dynamic landed target")?;
            if !seed_oracle_precedes(
                &events,
                position,
                round,
                task_id,
                &payload.target,
                &payload.sha256,
            ) {
                bail!(
                    "dynamic SeedRelocated 缺 earlier matching SeedOracleVerified: {}",
                    event.event_id
                );
            }
            let recorded = events[position + 1..].iter().any(|candidate| {
                candidate.kind == "TaskRecorded"
                    && candidate.actor == "runtime:orch"
                    && candidate.round.as_deref() == Some(round)
                    && candidate.task_id.as_deref() == Some(task_id)
                    && candidate
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("postMergeGates"))
                        .and_then(serde_json::Value::as_str)
                        == Some("all-green")
            });
            if !recorded {
                continue;
            }
            if payload.freeze_policy == SeedFreezePolicy::Evolvable {
                continue;
            }
            if let Some(existing) = anchors.get_mut(&payload.target) {
                if existing.digest.as_deref() != Some(&payload.sha256) {
                    bail!(
                        "dynamic SeedRelocated 不得覆盖既有 frozen anchor（须 FrozenContractSuperseded）: {}",
                        payload.target
                    );
                }
                if existing.effective.kind == "seed-relocated" {
                    existing.effective.event_id = Some(event.event_id.clone());
                }
                continue;
            }
            anchors.insert(
                payload.target.clone(),
                LandedAnchor {
                    digest: Some(payload.sha256.clone()),
                    effective: card::FrozenEffectiveAnchor {
                        kind: "seed-relocated".to_string(),
                        event_id: Some(event.event_id.clone()),
                        baseline_tree_sha: None,
                        sha256: Some(payload.sha256.clone()),
                    },
                    original_event_id: event.event_id.clone(),
                    original_sha256: payload.sha256,
                    grandfathered_drift: false,
                },
            );
        } else if event.kind == "FrozenContractSuperseded" {
            let payload = crate::verify::validate_frozen_contract_supersession_delta_for_replay(
                root, main_oid, &events, position,
            )
            .context("FrozenContractSuperseded delta 非 canonical signed Effective fact")?;
            if !is_canonical_sha256(&payload.new_file_sha256) {
                bail!("FrozenContractSuperseded newFileSha256 非规范");
            }
            let anchor = anchors.get_mut(&payload.target).with_context(|| {
                format!(
                    "FrozenContractSuperseded target 无 genesis: {}",
                    payload.target
                )
            })?;
            if anchor.effective != payload.effective_anchor
                || anchor.digest.as_deref() != Some(&payload.old_file_sha256)
            {
                bail!(
                    "FrozenContractSuperseded effective-anchor chain fork/backward: {}",
                    payload.target
                );
            }
            anchor.digest = Some(payload.new_file_sha256.clone());
            anchor.effective = card::FrozenEffectiveAnchor {
                kind: "frozen-contract-superseded".to_string(),
                event_id: Some(event.event_id.clone()),
                baseline_tree_sha: None,
                sha256: Some(payload.new_file_sha256),
            };
        }
    }
    if let Some((authorization, card)) =
        crate::verify::pending_frozen_record_context_from_committed_main(root, main_oid, &events)?
    {
        let mut declarations = card.meta.frozen_contract_supersessions;
        declarations.sort_by(|left, right| left.target.cmp(&right.target));
        for declaration in declarations {
            if !crate::verify::declared_reviews_match_bindings(
                &declaration.reviews,
                &authorization.reviews,
            ) {
                bail!("pending FrozenContractSuperseded reviews 未绑定 root PASS");
            }
            let anchor = anchors.get(&declaration.target).with_context(|| {
                format!(
                    "pending supersession target 无 landed genesis: {}",
                    declaration.target
                )
            })?;
            validate_declaration_against_snapshots(
                root,
                &declaration,
                &authorization.expected_main_sha,
                main_oid,
                anchor,
                &events,
            )?;
            anchors
                .get_mut(&declaration.target)
                .context("pending supersession target disappeared")?
                .digest = Some(declaration.new_file_sha256);
        }
    }
    Ok((anchors, events))
}

enum LegacyBaselineRecovery {
    Declared(binding::LandedSeedBaselineDescriptor),
    AuthenticatedlyAbsent,
    NoLegacyHistory,
}

fn authenticated_closed_legacy_baseline_descriptor(
    root: &Path,
    main_oid: &str,
    events: &[EventRecord],
) -> Result<LegacyBaselineRecovery> {
    let mut rounds = BTreeSet::new();
    for event in events.iter().filter(|event| event.kind == "RoundClosed") {
        let round = event
            .round
            .as_deref()
            .context("committed RoundClosed 缺 round")?;
        let number = round
            .strip_prefix('r')
            .and_then(|value| value.parse::<u64>().ok())
            .with_context(|| format!("committed RoundClosed round 非 r<数字>: {round:?}"))?;
        rounds.insert((number, round.to_string()));
    }
    if rounds.is_empty() {
        return Ok(LegacyBaselineRecovery::NoLegacyHistory);
    }
    for (_, round) in rounds.into_iter().rev() {
        let ir_path = format!("coordination/rounds/{round}/ROUND-IR.yaml");
        if !gitx::tree_path_exists(root, main_oid, &ir_path)? {
            bail!("committed closed round {round} 缺 ROUND-IR，拒绝向更老历史降级");
        }
        let ir_bytes = gitx::show_bytes(root, main_oid, &ir_path)?;
        let ir_text = std::str::from_utf8(&ir_bytes).context("closed ROUND-IR 非 UTF-8")?;
        let ir = crate::plan::parse_signed_round_ir(ir_text)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("解析 committed closed round {round} ROUND-IR 失败"))?;
        if ir.round != round {
            bail!("committed closed round {round} ROUND-IR round 漂移");
        }
        if ir.schema_version > 2 {
            continue;
        }
        let round_closes = events
            .iter()
            .filter(|event| {
                event.kind == "RoundClosed" && event.round.as_deref() == Some(round.as_str())
            })
            .collect::<Vec<_>>();
        if round_closes.is_empty() {
            continue;
        }
        if round_closes.len() != 1
            || round_closes[0].actor != "runtime:orch"
            || round_closes[0].task_id.is_some()
        {
            bail!("latest closed legacy round {round} 的 RoundClosed 非唯一 canonical");
        }
        let digest = crate::plan::validation_digest(&ir);
        let mut validations = Vec::new();
        for event in events.iter().filter(|event| {
            event.kind == "TaskValidated" && event.round.as_deref() == Some(round.as_str())
        }) {
            validations.push((
                event,
                crate::plan::decode_runtime_task_validated(event, &round)
                    .with_context(|| format!("latest closed legacy round {round} validation 非 canonical"))?,
            ));
        }
        let Some((_, high)) = validations.last() else {
            bail!("latest closed legacy round {round} 缺 production TaskValidated");
        };
        if high.ir_revision != ir.revision || high.validation_digest != digest {
            bail!("latest closed legacy round {round} ROUND-IR/high-water 漂移");
        }
        if crate::plan::matching_user_plan_signoff_position(
            events,
            &round,
            ir.revision,
            &digest,
        )?
        .is_none()
        {
            bail!("latest closed legacy round {round} 缺 matching PlanSignedOff");
        }
        if !is_canonical_sha256(&ir.source_bindings.binding_sha256) {
            bail!("latest closed legacy round {round} binding digest 非 canonical");
        }
        let historical_binding = ancestor_path_bytes_by_sha256(
            root,
            main_oid,
            "coordination/PROJECT-BINDING.yaml",
            &ir.source_bindings.binding_sha256,
        )?;
        let parsed = binding::parse_binding_bytes(&historical_binding)
            .map_err(anyhow::Error::msg)
            .context("authenticated legacy binding 非 canonical")?;
        if let Some(descriptor) = parsed.oracle.landed_seed_baseline {
            return Ok(LegacyBaselineRecovery::Declared(descriptor));
        }
        return Ok(LegacyBaselineRecovery::AuthenticatedlyAbsent);
    }
    Ok(LegacyBaselineRecovery::NoLegacyHistory)
}

fn planner_adjudication_round(path: &str) -> Result<&str> {
    canonical_repo_relative(path, "planner adjudication path")?;
    let mut parts = path.split('/');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (
            Some("coordination"),
            Some("rounds"),
            Some(round),
            Some("planning"),
            Some(file_name),
            None,
        ) if round.strip_prefix('r').is_some_and(|number| {
            !number.is_empty()
                && !number.starts_with('0')
                && number.bytes().all(|byte| byte.is_ascii_digit())
        }) && !file_name.trim().is_empty() =>
        {
            Ok(round)
        }
        _ => bail!(
            "planner adjudication path 必须精确位于 coordination/rounds/<round>/planning/: {path:?}"
        ),
    }
}

fn committed_regular_blob_bytes(
    root: &Path,
    commit_oid: &str,
    path: &str,
    label: &str,
) -> Result<Vec<u8>> {
    if !is_full_commit_oid(commit_oid) {
        bail!("{label} 只接受完整 commit OID");
    }
    canonical_repo_relative(path, label)?;
    let literal_pathspec = format!(":(literal){path}");
    let tree = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-tree",
            "--full-tree",
            "-z",
            commit_oid,
            "--",
            &literal_pathspec,
        ])
        .output()
        .with_context(|| format!("查询 {label} candidate tree entry 失败: {path}"))?;
    if !tree.status.success() {
        bail!(
            "查询 {label} candidate tree entry 失败: {}",
            String::from_utf8_lossy(&tree.stderr).trim()
        );
    }
    let entries = tree
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.len() != 1 {
        bail!("{label} 必须在 candidate tree 中恰有一个 tracked entry: {path}");
    }
    let entry = std::str::from_utf8(entries[0])
        .with_context(|| format!("{label} ls-tree entry 非 UTF-8"))?;
    let (header, entry_path) = entry
        .split_once('\t')
        .with_context(|| format!("{label} ls-tree entry 格式错误"))?;
    let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() != 3
        || !matches!(fields[0], "100644" | "100755")
        || fields[1] != "blob"
        || !is_full_commit_oid(fields[2])
        || entry_path != path
    {
        bail!("{label} 必须是 candidate tree 中的 regular tracked blob: {path}");
    }
    let blob = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "blob", fields[2]])
        .output()
        .with_context(|| format!("读取 {label} candidate blob 失败: {path}"))?;
    if !blob.status.success() {
        bail!(
            "读取 {label} candidate blob 失败: {}",
            String::from_utf8_lossy(&blob.stderr).trim()
        );
    }
    Ok(blob.stdout)
}

/// Validate the planner-adjudicated authorization arm against two immutable
/// Git snapshots.
///
/// The adjudication is read as a regular blob from `effective_oid`; the live
/// worktree is never consulted. Every removed assertion must exist only in the
/// old frozen-contract text, while its distinct retained-coverage marker must
/// exist in the new text. Both object IDs must be full commit IDs so a moving
/// ref cannot change the evidence during validation.
pub fn validate_planner_adjudicated_supersession(
    root: &Path,
    declaration: &card::FrozenContractSupersession,
    prior_main_oid: &str,
    effective_oid: &str,
) -> Result<()> {
    if !is_full_commit_oid(prior_main_oid) || !is_full_commit_oid(effective_oid) {
        bail!("planner-adjudicated supersession 只接受完整 commit OID");
    }
    let authorization = match (
        declaration.blocked_attempt.as_ref(),
        declaration.replacement.as_ref(),
        declaration.authorization.as_ref(),
    ) {
        (None, None, Some(authorization)) => authorization,
        _ => bail!("planner-adjudicated runtime 校验收到混合或不完整授权变体"),
    };
    if authorization.kind != "planner-adjudicated"
        || authorization.user_authorization.actor != "user"
        || authorization.user_authorization.date.trim().is_empty()
        || authorization.user_authorization.date.trim() != authorization.user_authorization.date
        || authorization.user_authorization.quote.trim().is_empty()
        || authorization.user_authorization.quote.trim() != authorization.user_authorization.quote
    {
        bail!("planner-adjudicated userAuthorization 字段形状非法");
    }
    planner_adjudication_round(&authorization.adjudication.path)?;
    if !is_canonical_sha256(&authorization.adjudication.sha256) {
        bail!("planner adjudication sha256 非 canonical");
    }

    let adjudication = committed_regular_blob_bytes(
        root,
        effective_oid,
        &authorization.adjudication.path,
        "planner adjudication",
    )?;
    if sha256_hex(&adjudication) != authorization.adjudication.sha256 {
        bail!("planner adjudication blob SHA-256 与 signed declaration 不一致");
    }

    let old = gitx::show_bytes(root, prior_main_oid, &declaration.target)
        .context("读取 planner supersession old frozen contract 失败")?;
    let new = gitx::show_bytes(root, effective_oid, &declaration.target)
        .context("读取 planner supersession new frozen contract 失败")?;
    if sha256_hex(&old) != declaration.old_file_sha256
        || sha256_hex(&new) != declaration.new_file_sha256
    {
        bail!("planner supersession frozen-contract whole-file SHA 与 fixed snapshots 不一致");
    }
    let old_text =
        std::str::from_utf8(&old).context("planner supersession old frozen contract 非 UTF-8")?;
    let new_text =
        std::str::from_utf8(&new).context("planner supersession new frozen contract 非 UTF-8")?;
    if authorization.removed_assertions.is_empty() {
        bail!("planner supersession removedAssertions 不得为空");
    }
    let mut assertions = BTreeSet::new();
    for removed in &authorization.removed_assertions {
        if removed.assertion.trim().is_empty()
            || removed.assertion.trim() != removed.assertion
            || removed.reason.trim().is_empty()
            || removed.reason.trim() != removed.reason
            || removed.retained_coverage.trim().is_empty()
            || removed.retained_coverage.trim() != removed.retained_coverage
            || removed.assertion == removed.retained_coverage
            || !assertions.insert(removed.assertion.as_str())
        {
            bail!("planner supersession removedAssertions 字段缺失、重复或自指");
        }
        if !old_text.contains(&removed.assertion) {
            bail!("planner supersession removed assertion 不存在于 old frozen contract");
        }
        if new_text.contains(&removed.assertion) {
            bail!("planner supersession removed assertion 仍存在于 new frozen contract");
        }
        if !new_text.contains(&removed.retained_coverage) {
            bail!("planner supersession retainedCoverage 不存在于 new frozen contract");
        }
    }
    Ok(())
}

fn first_byte_mismatch(left: &[u8], right: &[u8]) -> Option<usize> {
    left.iter()
        .zip(right)
        .position(|(left, right)| left != right)
        .or_else(|| (left.len() != right.len()).then_some(left.len().min(right.len())))
}

fn validate_structured_evolution_against_snapshots(
    declaration: &card::FrozenContractSupersession,
    evolution: &card::FrozenStructuredEvolution,
    old: &[u8],
    new: &[u8],
) -> Result<()> {
    if evolution.units.is_empty() {
        bail!("structured evolution units 不得为空");
    }

    if evolution.units.len() == 1 {
        let range = evolution.units[0].old_range;
        if range.start == 0 && usize::try_from(range.end).ok() == Some(old.len()) {
            bail!("structured evolution 拒绝单个整文件 catch-all unit");
        }
    }

    let mut reconstructed = Vec::new();
    let mut cursor = 0usize;
    let mut previous_start: Option<usize> = None;
    let mut deleted_assertions = BTreeSet::new();
    for (index, unit) in evolution.units.iter().enumerate() {
        let start = usize::try_from(unit.old_range.start).with_context(|| {
            format!("structured evolution unit[{index}] oldRange.start 超出平台范围")
        })?;
        let end = usize::try_from(unit.old_range.end).with_context(|| {
            format!("structured evolution unit[{index}] oldRange.end 超出平台范围")
        })?;
        if start > end || end > old.len() {
            bail!("structured evolution unit[{index}] oldRange 越界或反向");
        }
        if previous_start.is_some_and(|previous| start <= previous) || start < cursor {
            bail!("structured evolution units 必须严格有序且互不重叠");
        }
        if !is_canonical_sha256(&unit.old_sha256) || sha256_hex(&old[start..end]) != unit.old_sha256
        {
            bail!("structured evolution unit[{index}] old window SHA-256 不匹配");
        }

        reconstructed.extend_from_slice(&old[cursor..start]);
        match &unit.edit {
            card::FrozenContractEdit::Replace { content } => {
                if content.is_empty() || content.as_bytes() == &old[start..end] {
                    bail!(
                        "structured evolution unit[{index}] replace 必须声明非空且真实变化的内容"
                    );
                }
                reconstructed.extend_from_slice(content.as_bytes());
            }
            card::FrozenContractEdit::Delete { removed_assertion } => {
                if start == end
                    || removed_assertion.trim().is_empty()
                    || removed_assertion.trim() != removed_assertion
                    || !deleted_assertions.insert(removed_assertion.as_str())
                    || !old[start..end]
                        .windows(removed_assertion.len())
                        .any(|window| window == removed_assertion.as_bytes())
                {
                    bail!("structured evolution unit[{index}] delete 未精确绑定唯一 old removedAssertion");
                }
            }
        }
        cursor = end;
        previous_start = Some(start);
    }
    reconstructed.extend_from_slice(&old[cursor..]);

    if let Some(offset) = first_byte_mismatch(&reconstructed, new) {
        bail!("structured evolution unexplained byte offset={offset}");
    }

    if !deleted_assertions.is_empty() {
        let authorization = declaration.authorization.as_ref().context(
            "structured evolution delete 必须绑定 planner-adjudicated removedAssertions",
        )?;
        let authorized = authorization
            .removed_assertions
            .iter()
            .map(|removed| removed.assertion.as_str())
            .collect::<BTreeSet<_>>();
        if authorized != deleted_assertions {
            bail!("structured evolution delete 与 removedAssertions 未逐条双向绑定");
        }
    }
    Ok(())
}

fn validate_declaration_against_snapshots(
    root: &Path,
    declaration: &card::FrozenContractSupersession,
    prior_main_oid: &str,
    effective_oid: &str,
    anchor: &LandedAnchor,
    events: &[EventRecord],
) -> Result<()> {
    if declaration.effective_anchor != anchor.effective
        || declaration.old_file_sha256 != anchor.digest.as_deref().unwrap_or("")
    {
        bail!("supersession declared old/effective anchor 与 canonical chain 不一致");
    }
    let old = gitx::show_bytes(root, prior_main_oid, &declaration.target)
        .context("读取 supersession old contract bytes 失败")?;
    let new = gitx::show_bytes(root, effective_oid, &declaration.target)
        .context("读取 supersession new contract bytes 失败")?;
    if sha256_hex(&old) != declaration.old_file_sha256
        || sha256_hex(&new) != declaration.new_file_sha256
    {
        bail!("supersession whole-file SHA 与精确快照不一致");
    }
    let old_text = std::str::from_utf8(&old).context("supersession old contract 非 UTF-8")?;
    let new_text = std::str::from_utf8(&new).context("supersession new contract 非 UTF-8")?;
    match declaration.evolution().map_err(anyhow::Error::msg)? {
        card::FrozenContractEvolution::LiteralSwap {
            old_literal_sha256,
            new_literal_sha256,
            subject_prefix,
        } => {
            let swap = single_hash_literal_swap(old_text, new_text)
                .context("literal shape: supersession 并非恰好一个 quoted SHA-256 literal swap")?;
            if swap.0 != old_literal_sha256 || swap.1 != new_literal_sha256 {
                bail!("supersession literal swap 与 typed declaration 不一致");
            }
            let prefix_bytes = usize::try_from(subject_prefix.bytes)
                .context("subjectPrefix.bytes 超出平台范围")?;
            let old_subject = gitx::show_bytes(root, prior_main_oid, &subject_prefix.path)
                .context("读取 old subject-prefix path 失败")?;
            let new_subject = gitx::show_bytes(root, effective_oid, &subject_prefix.path)
                .context("读取 new subject-prefix path 失败")?;
            if old_subject.len() < prefix_bytes
                || new_subject.len() < prefix_bytes
                || sha256_hex(&old_subject[..prefix_bytes]) != old_literal_sha256
                || sha256_hex(&new_subject[..prefix_bytes]) != new_literal_sha256
            {
                bail!("subject-prefix 摘要与 literal replacement 不一致");
            }
        }
        card::FrozenContractEvolution::Structured(evolution) => {
            validate_structured_evolution_against_snapshots(
                declaration,
                evolution,
                old_text.as_bytes(),
                new_text.as_bytes(),
            )?;
        }
    }
    validate_relocation_event(
        events,
        &declaration.original_seed_relocated.event_id,
        event_by_id(events, &declaration.original_seed_relocated.event_id)?
            .round
            .as_deref()
            .context("original SeedRelocated 缺 round")?,
        event_by_id(events, &declaration.original_seed_relocated.event_id)?
            .task_id
            .as_deref()
            .context("original SeedRelocated 缺 taskId")?,
        &declaration.target,
        &declaration.original_seed_relocated.sha256,
    )?;
    if declaration.original_seed_relocated.event_id != anchor.original_event_id
        || declaration.original_seed_relocated.sha256 != anchor.original_sha256
    {
        bail!("supersession original SeedRelocated anchor 不匹配 genesis");
    }
    let recovery_authorized = match (
        declaration.blocked_attempt.as_ref(),
        declaration.replacement.as_ref(),
        declaration.authorization.as_ref(),
    ) {
        (Some(blocked_anchor), Some(replacement), None) => {
            let blocked = event_by_id(events, &blocked_anchor.event_id)?;
            if blocked.kind != "AttemptBlocked"
                || blocked.actor != "runtime:orch"
                || blocked.round.as_deref() != Some(&blocked_anchor.round)
                || blocked.task_id.as_deref() != Some(&blocked_anchor.task_id)
                || payload_string(blocked, "attemptId")? != blocked_anchor.attempt_id
            {
                bail!("supersession AttemptBlocked anchor 不匹配");
            }
            validate_recorded_event(
                events,
                &replacement.task_recorded_event_id,
                &replacement.round,
                &replacement.task_id,
            )?;
            true
        }
        (None, None, Some(_)) => {
            validate_planner_adjudicated_supersession(
                root,
                declaration,
                prior_main_oid,
                effective_oid,
            )?;
            false
        }
        _ => bail!("supersession authorization 变体混合或不完整"),
    };
    match anchor.effective.kind.as_str() {
        "seed-relocated" => {
            if recovery_authorized {
                frozen_contract_supersession_authorized(
                    "runtime:orch",
                    true,
                    Some(&anchor.original_sha256),
                    &declaration.old_file_sha256,
                    anchor.digest.as_deref().unwrap_or(""),
                )
                .map_err(anyhow::Error::msg)?;
            } else {
                frozen_contract_anchor_digests_match(
                    Some(&anchor.original_sha256),
                    &declaration.old_file_sha256,
                    anchor.digest.as_deref().unwrap_or(""),
                )
                .map_err(anyhow::Error::msg)?;
            }
        }
        "migration-baseline" => {
            if !anchor.grandfathered_drift
                || anchor.effective.baseline_tree_sha.is_none()
                || anchor.digest.as_deref() != Some(&declaration.old_file_sha256)
            {
                bail!("migration-baseline supersession 未显式 grandfather 或 old 不匹配");
            }
        }
        "frozen-contract-superseded" => {
            if anchor.digest.as_deref() != Some(&declaration.old_file_sha256) {
                bail!("repeat supersession old SHA 未链接上一 effective event");
            }
        }
        "migration-tombstone" => bail!("tombstoned frozen contract 不得复活"),
        other => bail!("unknown effective anchor kind: {other}"),
    }
    Ok(())
}

/// Validate one signed declaration after the candidate has become the exact
/// effective main snapshot.  Close/verify share this entry point with the
/// pre-merge guard so authorization cannot silently shrink at emission time.
pub fn validate_frozen_contract_declaration(
    root: &Path,
    declaration: &card::FrozenContractSupersession,
    prior_main_oid: &str,
    effective_main_oid: &str,
) -> Result<()> {
    let (anchors, events) = load_signed_baseline(root, prior_main_oid)?;
    let anchor = anchors.get(&declaration.target).with_context(|| {
        format!(
            "supersession target 无 landed genesis: {}",
            declaration.target
        )
    })?;
    validate_declaration_against_snapshots(
        root,
        declaration,
        prior_main_oid,
        effective_main_oid,
        anchor,
        &events,
    )
}

fn validate_candidate_seed_target_modes(
    root: &Path,
    c: &card::Card,
    candidate_oid: &str,
) -> Result<()> {
    for seed in &c.meta.seeds {
        let literal = format!(":(literal){}", seed.target);
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "ls-tree",
                "--full-tree",
                "-z",
                candidate_oid,
                "--",
                literal.as_str(),
            ])
            .output()
            .with_context(|| format!("检查 candidate seed target mode 失败: {}", seed.target))?;
        if !output.status.success() {
            bail!(
                "检查 candidate seed target mode 失败({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let entries = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>();
        let [entry] = entries.as_slice() else {
            bail!("candidate seed target 必须恰有一个 tree entry: {}", seed.target);
        };
        let entry = std::str::from_utf8(entry).context("candidate seed ls-tree 非 UTF-8")?;
        let (header, path) = entry
            .split_once('\t')
            .context("candidate seed ls-tree entry 格式错误")?;
        let mut fields = header.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755")
            || kind != "blob"
            || object.len() != 40
            || path != seed.target
        {
            bail!("candidate seed target 必须是 regular tracked blob: {}", seed.target);
        }
    }
    Ok(())
}

/// Validate this card's seed containment and regular-blob modes at a fixed candidate.
/// Both inputs must be full commit OIDs. Prior Recorded targets confer no permanent
/// veto; the mechanical check independently enforces this card's exact seed bytes.
pub fn validate_seed_paths_for_candidate(
    root: &Path,
    c: &card::Card,
    main_oid: &str,
    candidate_oid: &str,
) -> Result<()> {
    if !is_full_commit_oid(main_oid) || !is_full_commit_oid(candidate_oid) {
        bail!("candidate-aware seed guard 只接受完整 commit OID");
    }
    validate_candidate_seed_target_modes(root, c, candidate_oid)?;
    validate_seed_path_shape(root, c)
}

fn inspect_target_path(base: &Path, relative: &Path, display: &str) -> Result<()> {
    let components = relative.components().collect::<Vec<_>>();
    let mut current = base.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            bail!("seed target 内部校验遇到非规范 component: {display:?}");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "seed target 禁止符号链接（含父目录）: {display} ({})",
                        current.display()
                    );
                }
                let is_last = index + 1 == components.len();
                if is_last && !metadata.is_file() {
                    bail!("seed target 已存在且不是 regular file: {display}");
                }
                if !is_last && !metadata.is_dir() {
                    bail!(
                        "seed target 父 component 不是目录: {display} ({})",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("检查 seed target 失败: {display} ({})", current.display())
                });
            }
        }
    }
    Ok(())
}

fn create_checked_target_parent(base: &Path, relative: &Path, display: &str) -> Result<PathBuf> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut current = base.to_path_buf();
    for component in parent.components() {
        let Component::Normal(part) = component else {
            bail!("seed target 父目录含非规范 component: {display:?}");
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "seed target 父 component 必须是真实目录: {display} ({})",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("创建 seed target 父目录失败: {}", current.display())
                        });
                    }
                }
                let metadata = fs::symlink_metadata(&current).with_context(|| {
                    format!("复查 seed target 父目录失败: {}", current.display())
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "seed target 新建父 component 被替换成非真实目录: {}",
                        current.display()
                    );
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("检查 seed target 父目录失败: {}", current.display()))
            }
        }
    }
    let resolved = fs::canonicalize(&current)
        .with_context(|| format!("解析 seed target 父目录失败: {}", current.display()))?;
    if !resolved.starts_with(base) {
        bail!(
            "seed target 父目录解析后逃出 worktree: {display} → {}",
            resolved.display()
        );
    }
    Ok(current)
}

/// Validate this card's seed source/target containment without resolving a movable Git ref.
/// Planning and candidate inspection share these path checks; the mechanical candidate
/// check separately compares each current seed with its immutable source bytes.
pub(crate) fn validate_seed_path_shape(root: &Path, c: &card::Card) -> Result<()> {
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let mut targets = HashSet::new();
    for seed in &c.meta.seeds {
        let src = canonical_repo_relative(&seed.src, "src")?;
        let target = canonical_repo_relative(&seed.target, "target")?;
        checked_regular_source(&canonical_root, &src, &seed.src)?;
        if !card::path_matches(&c.meta.write_set, &seed.target) {
            bail!("seed target 不在任务 writeSet: {}", seed.target);
        }
        if card::path_matches(&c.meta.frozen_paths, &seed.target) {
            bail!("seed target 命中任务 frozenPaths: {}", seed.target);
        }
        if targets
            .iter()
            .any(|existing: &PathBuf| target.starts_with(existing) || existing.starts_with(&target))
        {
            bail!("任务卡 seed targets 不得重复或互为祖先: {}", seed.target);
        }
        if !targets.insert(target) {
            bail!("任务卡含重复 seed target: {}", seed.target);
        }
    }
    Ok(())
}

fn validate_materialized_seed_replay(canonical_root: &Path, c: &card::Card) -> Result<()> {
    for seed in &c.meta.seeds {
        let source = read_bound_seed_bytes(canonical_root, &seed.src)?;
        let source_digest = sha256_hex(&source);
        let landed_digest = current_landed_digest(canonical_root, &seed.target)?;
        if landed_digest
            .as_deref()
            .is_some_and(|landed| landed != source_digest.as_str())
        {
            bail!(
                "新 seed 试图覆盖已落位 target: {}，当前 {:?}，seed.src {}",
                seed.target,
                landed_digest,
                source_digest
            );
        }
    }
    Ok(())
}

/// Preserve plan-time seed creation/replay safety without resolving Git refs.
///
/// In addition to containment, an already materialized target must be
/// byte-identical to its immutable source. This compatibility helper reconstructs
/// historical planning; it does not grant permanent authority over other cards' targets.
pub(crate) fn validate_seed_paths_for_authorized_plan(root: &Path, c: &card::Card) -> Result<()> {
    validate_seed_path_shape(root, c)?;
    let canonical_root = canonical_directory(root, "仓库 root")?;
    validate_materialized_seed_replay(&canonical_root, c)
}

/// Validate only the evolvable schema 3 seed boundary.
///
/// Source identity, containment, target write authorization, and symlink
/// rejection remain strict. Historical landed-target permanence is omitted.
pub(crate) fn validate_seed_paths_for_v3(root: &Path, c: &card::Card) -> Result<()> {
    if c.meta.schema_version != Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
        bail!("v3 seed validator requires schemaVersion: 3 card");
    }
    validate_seed_path_shape(root, c)
}

fn card_round_is_closed_at_main(root: &Path, main_oid: &str, c: &card::Card) -> Result<bool> {
    let Some(round) = c.meta.round.as_deref() else {
        return Ok(false);
    };
    let ledger_path = format!("coordination/rounds/{round}/events.jsonl");
    if !gitx::tree_path_exists(root, main_oid, &ledger_path)? {
        return Ok(false);
    }
    let bytes = gitx::show_bytes(root, main_oid, &ledger_path)?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("历史轮 ledger 非 UTF-8: {ledger_path}"))?;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: EventRecord = serde_json::from_str(line)
            .with_context(|| format!("历史轮 ledger 坏行: {ledger_path}:{}", index + 1))?;
        if event.kind == "RoundClosed"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.is_none()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 在任何 worktree/ledger/spawn 动作前验证种子路径合同。
///
/// `src` 必须是仓库内、全链无 symlink 的 regular file；`target` 必须是规范相对路径，
/// 且属于任务 writeSet、不得命中任务 frozenPaths。重复 target 也会被拒绝，防止后一个
/// seed 静默覆盖前一个。schema3 不建立跨卡永久 landed 准入；收取期由机检以固定候选
/// 快照核本卡种子字节。legacy 只读重建保留 [`validate_seed_paths_for_authorized_plan`]
/// 的自身种子 materialized replay 校验，不把历史字节约束变成当前写入政策。
pub fn validate_seed_paths(root: &Path, c: &card::Card) -> Result<()> {
    if c.meta.schema_version == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
        return validate_seed_paths_for_v3(root, c);
    }
    validate_seed_path_shape(root, c)?;
    if !gitx::branch_exists(root, "main") {
        return Ok(());
    }
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let git_toplevel = gitx::rev_parse(root, "--show-toplevel")?;
    let canonical_toplevel = fs::canonicalize(&git_toplevel)
        .with_context(|| format!("解析 Git toplevel 失败: {git_toplevel}"))?;
    if canonical_root != canonical_toplevel {
        // Synthetic roots may live beneath this repository's ignored target/
        // tree.  Git searches parent directories, but their `main` and landed
        // manifest do not govern the nested fixture.
        return Ok(());
    }
    let main_oid = gitx::rev_parse(root, "refs/heads/main^{commit}")?;
    // The migration baseline is deliberately non-retroactive.  Read-only
    // recompilation of an already closed round must keep that round's signed
    // planning semantics; otherwise every archived card would be rejudged as
    // a brand-new seed attempt against a baseline created years later.
    if card_round_is_closed_at_main(root, &main_oid, c)? {
        return Ok(());
    }
    // Keep this compatibility entry point cheap and pre-action safe: planning,
    // copying, and red replay all call it.  An already materialized seed target
    // may only be replayed byte-for-byte. This is compatibility for the card's
    // own materialized seeds; no permanent cross-card census is performed.
    validate_materialized_seed_replay(&canonical_root, c)
}

/// 将任务卡 seeds 安全落位到已存在的 worktree。
///
/// 所有源文件会在任何目标写入前完成稳定身份读取；所有现存目标链也会先做无 symlink
/// 预检。单文件通过同目录临时文件 + rename 落位，最终 component 即使被并发换成 symlink
/// 也只会被原子替换，不会跟随写入其指向对象。
pub fn copy_seeds_contained(root: &Path, worktree: &Path, c: &card::Card) -> Result<()> {
    validate_seed_paths(root, c)?;
    let canonical_root = canonical_directory(root, "仓库 root")?;
    let canonical_worktree = canonical_directory(worktree, "oracle worktree")?;

    let mut loaded = Vec::with_capacity(c.meta.seeds.len());
    for seed in &c.meta.seeds {
        let src = canonical_repo_relative(&seed.src, "src")?;
        let target = canonical_repo_relative(&seed.target, "target")?;
        let (bytes, permissions) = read_checked_seed(&canonical_root, &src, &seed.src)?;
        loaded.push((seed, target, bytes, permissions));
    }
    for (seed, target, _, _) in &loaded {
        inspect_target_path(&canonical_worktree, target, &seed.target)?;
    }

    for (seed, target, bytes, permissions) in loaded {
        let parent = create_checked_target_parent(&canonical_worktree, &target, &seed.target)?;
        inspect_target_path(&canonical_worktree, &target, &seed.target)?;
        let destination = canonical_worktree.join(&target);
        let temporary = parent.join(format!(
            ".{}",
            crate::util::unique_scratch_name("orch-seed-copy")
        ));
        let write_result = (|| -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .with_context(|| format!("创建 seed 临时文件失败: {}", temporary.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("写 seed 临时文件失败: {}", temporary.display()))?;
            file.sync_all()
                .with_context(|| format!("同步 seed 临时文件失败: {}", temporary.display()))?;
            drop(file);
            fs::set_permissions(&temporary, permissions)
                .with_context(|| format!("设置 seed 权限失败: {}", temporary.display()))?;

            // rename 前再次确认父链，避免检查后被静态 symlink 替换。
            create_checked_target_parent(&canonical_worktree, &target, &seed.target)?;
            inspect_target_path(&canonical_worktree, &target, &seed.target)?;
            fs::rename(&temporary, &destination)
                .with_context(|| format!("落位 seed 失败: {} → {}", seed.src, seed.target))?;
            Ok(())
        })();
        if write_result.is_err() {
            fs::remove_file(&temporary).ok();
        }
        write_result?;
    }
    Ok(())
}

pub fn parse_suite_counts(log: &str) -> Option<SuiteCounts> {
    // 取最后一个含「Tests 」的行（gate 日志=行首缩进；REPORT §3 叙事=行中缀，都认）。
    // 「Test Files」「(tests 17ms)」等干扰行不含大写开头的「Tests 」子串。
    let raw = log.lines().filter(|l| l.contains("Tests ")).next_back()?;
    let line = &raw[raw.find("Tests ")?..];
    let total = line
        .rsplit_once('(')
        .and_then(|(_, r)| r.split(')').next())
        .and_then(|n| n.trim().parse().ok())?;
    let num_before = |kw: &str| -> usize {
        line.split(kw)
            .next()
            .and_then(|before| before.split_whitespace().next_back())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    Some(SuiteCounts {
        failed: num_before("failed"),
        passed: num_before("passed"),
        total,
    })
}

pub fn parse_file_counts(log: &str, target: &str) -> Option<FileCounts> {
    // 文件计数行 = 含目标路径 + 「(N test…」（单用例是「(1 test)」单数）。
    // 必须校验括号后首 token 是数字——失败详情区的 `❯ target:15:5` 位置行会伪装成匹配（r6 实测踩坑）
    let line = log.lines().rev().find(|l| {
        l.contains(target)
            && l.contains(" test")
            && l.split_once('(')
                .and_then(|(_, r)| r.split_whitespace().next())
                .is_some_and(|tok| !tok.is_empty() && tok.chars().all(|c| c.is_ascii_digit()))
    })?;
    let inner = line.split_once('(').map(|(_, r)| r)?;
    let tests: usize = inner.split_whitespace().next()?.parse().ok()?;
    let failed = if let Some((_, after)) = inner.split_once('|') {
        after
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    Some(FileCounts { tests, failed })
}

pub fn parse_failed_cases(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| l.trim_start().starts_with('×'))
        .map(|l| l.trim().trim_start_matches('×').trim().to_string())
        .collect()
}

/// cargo workspace 汇总计数：每个测试二进制各有一行，必须跨行求和。
/// 至少解析出一个真实数字才算计数行——叙述性文本（如「无任何 `test result:` 行」这句话
/// 本身含子串）不得伪装成计数行（E13，r8/B12 误杀实测）。
pub fn parse_cargo_suite_counts(log: &str) -> Option<SuiteCounts> {
    let mut found = false;
    let (mut failed, mut passed) = (0usize, 0usize);
    for line in log.lines().filter(|line| line.contains("test result:")) {
        let number_before = |keyword: &str| -> Option<usize> {
            line.split(keyword)
                .next()
                .and_then(|before| before.split_whitespace().next_back())
                .and_then(|number| number.parse().ok())
        };
        let f = number_before("failed");
        let p = number_before("passed");
        if f.is_none() && p.is_none() {
            continue; // 无任何真实数字：非计数行
        }
        found = true;
        failed += f.unwrap_or(0);
        passed += p.unwrap_or(0);
    }
    found.then_some(SuiteCounts {
        failed,
        passed,
        total: failed + passed,
    })
}

/// cargo 失败行形如 `test module::case ... FAILED`；保留首次出现顺序并去重。
pub fn parse_cargo_failed_cases(log: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut cases = Vec::new();
    for line in log.lines() {
        let trimmed = line.trim();
        let Some(case) = trimmed
            .strip_prefix("test ")
            .and_then(|value| value.strip_suffix(" ... FAILED"))
        else {
            continue;
        };
        if seen.insert(case.to_string()) {
            cases.push(case.to_string());
        }
    }
    cases
}

fn cargo_test_names(seed_source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut awaiting_fn = false;
    for line in seed_source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#[test]") {
            awaiting_fn = true;
        }
        if !awaiting_fn {
            continue;
        }
        let tokens = trimmed.split_whitespace().collect::<Vec<_>>();
        let Some(fn_index) = tokens.iter().position(|token| *token == "fn") else {
            continue;
        };
        let Some(raw_name) = tokens.get(fn_index + 1) else {
            continue;
        };
        let name = raw_name.split(['(', '<']).next().unwrap_or_default().trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
        awaiting_fn = false;
    }
    names
}

/// Count expected Cargo tests and matching failure names from seed source.
/// This static inventory does not prove execution or passing outcomes; live
/// assertion observations must use `observed_cargo_seed_counts` instead.
pub fn cargo_file_counts(seed_source: &str, failed_cases: &[String]) -> FileCounts {
    let test_names = cargo_test_names(seed_source);
    let failed = failed_cases
        .iter()
        .filter(|case| {
            test_names
                .iter()
                .any(|name| *case == name.as_str() || case.ends_with(&format!("::{name}")))
        })
        .count();
    FileCounts {
        tests: test_names.len(),
        failed,
    }
}

/// Expected seed cases and their signed Cargo integration-test target.
/// This inventory identifies required observations; it never supplies passing
/// results for tests that Cargo did not report running.
#[derive(Debug, Clone)]
pub struct CargoSeedRunSpec {
    /// Canonical repository-relative integration target from the signed seed.
    pub target: String,
    /// Test names extracted from that seed's source, each requiring one result.
    pub test_names: Vec<String>,
}

#[derive(Debug)]
struct ObservedCargoRun {
    target: String,
    binary: String,
    declared: Option<usize>,
    outcomes: Vec<(String, Option<bool>)>,
    summary: Option<(usize, usize, usize)>,
}

fn cargo_observed_summary(line: &str) -> Option<(usize, usize, usize)> {
    let (success, body) = line
        .strip_prefix("test result: ok. ")
        .map(|body| (true, body))
        .or_else(|| {
            line.strip_prefix("test result: FAILED. ")
                .map(|body| (false, body))
        })?;
    let count = |label: &str| -> Option<usize> {
        let matching = body
            .split(';')
            .filter(|part| part.split_whitespace().last() == Some(label))
            .collect::<Vec<_>>();
        let [part] = matching.as_slice() else {
            return None;
        };
        let words = part.split_whitespace().collect::<Vec<_>>();
        let [number, _] = words.as_slice() else {
            return None;
        };
        number.parse().ok()
    };
    let summary = (count("passed")?, count("failed")?, count("ignored")?);
    (success == (summary.1 == 0)).then_some(summary)
}

/// Count seed outcomes only from complete, uniquely attributable Cargo runs.
/// Each signed integration target needs its own `Running` boundary, matching
/// test binary, named `ok`/`FAILED` results and consistent closing summary.
/// Missing, ignored, filtered, duplicate or truncated observations are errors,
/// never inferred passes. This does not alter compile-red or `Measured` JSON.
/// Cargo text has no package ID; the live oracle also checks source ownership
/// in the workspace before publishing these counts.
/// Other targets' runtime chatter is not seed evidence. This function counts
/// selected targets; it does not decide whether the entire Cargo command passed.
pub fn observed_cargo_seed_counts(
    log: &str,
    seeds: &[CargoSeedRunSpec],
) -> Result<SuiteCounts, String> {
    let missing = |detail: String| format!("seed-not-observed: {detail}");
    let ambiguous = |detail: String| format!("seed-not-attributable: {detail}");
    if seeds.is_empty() {
        return Err(missing("no signed seed targets".into()));
    }
    if !log.ends_with('\n') {
        return Err(missing(
            "Cargo log is empty or lacks a complete final line".into(),
        ));
    }
    let normalized = strip_ansi_csi(log);
    let selected_labels = seeds
        .iter()
        .filter_map(|seed| {
            Path::new(&seed.target)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .map(|name| format!("tests/{name}"))
        .collect::<HashSet<_>>();
    let mut runs: Vec<ObservedCargoRun> = Vec::new();
    let mut current: Option<usize> = None;
    for line in normalized.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix("Running ") {
            let boundary = header.rsplit_once(" (").and_then(|(target, binary)| {
                binary.strip_suffix(')').map(|binary| (target, binary))
            });
            let Some((target, binary)) = boundary else {
                let selected = selected_labels
                    .iter()
                    .any(|label| header.split_whitespace().next() == Some(label.as_str()));
                if selected || current.is_some_and(|index| runs[index].summary.is_none()) {
                    return Err(ambiguous(format!(
                        "malformed Cargo seed target boundary: {line}"
                    )));
                }
                current = None;
                continue;
            };
            if !selected_labels.contains(target) {
                if current.is_some_and(|index| runs[index].summary.is_none()) {
                    return Err(missing(format!(
                        "{} was interrupted before its summary",
                        runs[current.unwrap()].target
                    )));
                }
                current = None;
                continue;
            }
            if current.is_some_and(|index| runs[index].summary.is_none()) {
                return Err(missing(format!(
                    "{target} began before the previous seed target summary"
                )));
            }
            runs.push(ObservedCargoRun {
                target: target.to_string(),
                binary: binary.to_string(),
                declared: None,
                outcomes: Vec::new(),
                summary: None,
            });
            current = Some(runs.len() - 1);
            continue;
        }
        if line.starts_with("Doc-tests ") {
            if current.is_some_and(|index| runs[index].summary.is_none()) {
                return Err(missing(
                    "doctests began before a Cargo target completed".into(),
                ));
            }
            current = None;
            continue;
        }
        let Some(index) = current else { continue };
        let run = &mut runs[index];
        if let Some(rest) = line.strip_prefix("running ") {
            let words = rest.split_whitespace().collect::<Vec<_>>();
            if words.len() == 2 && matches!(words[1], "test" | "tests") {
                if run.declared.is_some() || run.summary.is_some() {
                    return Err(ambiguous(format!(
                        "duplicate run declaration for {}",
                        run.target
                    )));
                }
                run.declared = Some(
                    words[0]
                        .parse()
                        .map_err(|_| ambiguous(format!("invalid run count: {line}")))?,
                );
            }
        } else if line.starts_with("test result:") {
            if run.summary.is_some() {
                return Err(ambiguous(format!("duplicate summary for {}", run.target)));
            }
            let summary = cargo_observed_summary(line).ok_or_else(|| {
                missing(format!("invalid or incomplete summary for {}", run.target))
            })?;
            if line.starts_with("test result: ok.") && summary.1 != 0 {
                return Err(ambiguous(format!(
                    "success summary reports failures for {}",
                    run.target
                )));
            }
            run.summary = Some(summary);
        } else if let Some((name, result)) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.split_once(" ... "))
        {
            let outcome = match result {
                "ok" => Some(Some(true)),
                "FAILED" => Some(Some(false)),
                value if value == "ignored" || value.starts_with("ignored,") => Some(None),
                _ => None,
            };
            if let Some(outcome) = outcome {
                if run.summary.is_some() || name.is_empty() {
                    return Err(ambiguous(format!("result outside an open target: {line}")));
                }
                run.outcomes.push((name.to_string(), outcome));
            }
        }
    }
    let mut counts = SuiteCounts {
        passed: 0,
        failed: 0,
        total: 0,
    };
    let mut seen_targets = HashSet::new();
    let mut seen_labels = HashSet::new();
    for seed in seeds {
        let parts = seed.target.split('/').collect::<Vec<_>>();
        let ["orch", "crates", package, "tests", file] = parts.as_slice() else {
            return Err(ambiguous(format!(
                "unsupported signed integration target {}",
                seed.target
            )));
        };
        let Some(stem) = file.strip_suffix(".rs").filter(|stem| !stem.is_empty()) else {
            return Err(ambiguous(format!("invalid seed target {}", seed.target)));
        };
        let safe_component = |value: &str| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        };
        if !safe_component(package)
            || !safe_component(stem)
            || !seen_targets.insert(seed.target.as_str())
        {
            return Err(ambiguous(format!(
                "duplicate or invalid seed target {}",
                seed.target
            )));
        }
        let label = format!("tests/{file}");
        if !seen_labels.insert(label.clone()) {
            return Err(ambiguous(format!(
                "{} shares a Cargo label with another seed target",
                seed.target
            )));
        }
        let matching = runs
            .iter()
            .filter(|run| run.target == label)
            .collect::<Vec<_>>();
        let run = match matching.as_slice() {
            [] => {
                return Err(missing(format!(
                    "{} has no Cargo target execution",
                    seed.target
                )))
            }
            [run] => *run,
            _ => {
                return Err(ambiguous(format!(
                    "{} has multiple Cargo target executions",
                    seed.target
                )))
            }
        };
        let binary = run.binary.rsplit(['/', '\\']).next().unwrap_or_default();
        let binary = binary.strip_suffix(".exe").unwrap_or(binary);
        let prefix = format!("{}-", stem.replace('-', "_"));
        if !binary.strip_prefix(&prefix).is_some_and(|hash| {
            !hash.is_empty() && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(ambiguous(format!(
                "{} has foreign test binary {}",
                seed.target, run.binary
            )));
        }
        let (passed, failed, ignored) = run
            .summary
            .ok_or_else(|| missing(format!("{} has no complete summary", seed.target)))?;
        let observed = (
            run.outcomes
                .iter()
                .filter(|(_, outcome)| *outcome == Some(true))
                .count(),
            run.outcomes
                .iter()
                .filter(|(_, outcome)| *outcome == Some(false))
                .count(),
            run.outcomes
                .iter()
                .filter(|(_, outcome)| outcome.is_none())
                .count(),
        );
        let total = passed
            .checked_add(failed)
            .and_then(|count| count.checked_add(ignored));
        if observed != (passed, failed, ignored) || total != run.declared || total.is_none() {
            return Err(missing(format!(
                "{} results and closing counts disagree",
                seed.target
            )));
        }
        if seed.test_names.is_empty() {
            return Err(missing(format!(
                "{} has no expected test cases",
                seed.target
            )));
        }
        let mut expected = HashSet::new();
        let mut matched = HashSet::new();
        for name in &seed.test_names {
            if name.trim().is_empty() || name.trim() != name || !expected.insert(name) {
                return Err(ambiguous(format!(
                    "{} has duplicate or invalid expected case {name}",
                    seed.target
                )));
            }
            let cases = run
                .outcomes
                .iter()
                .enumerate()
                .filter(|(_, (actual, _))| {
                    actual == name
                        || (!name.contains("::") && actual.ends_with(&format!("::{name}")))
                })
                .collect::<Vec<_>>();
            let (index, (_, outcome)) = match cases.as_slice() {
                [] => {
                    return Err(missing(format!(
                        "{}::{name} has no observed result",
                        seed.target
                    )))
                }
                [entry] => *entry,
                _ => {
                    return Err(ambiguous(format!(
                        "{}::{name} has repeated or ambiguous results",
                        seed.target
                    )))
                }
            };
            if !matched.insert(index) {
                return Err(ambiguous(format!(
                    "{} reuses a result for {name}",
                    seed.target
                )));
            }
            match outcome {
                Some(true) => counts.passed += 1,
                Some(false) => counts.failed += 1,
                None => return Err(missing(format!("{}::{name} was ignored", seed.target))),
            }
        }
        if matched.len() != run.outcomes.len() {
            return Err(ambiguous(format!(
                "{} contains cases absent from the signed seed inventory",
                seed.target
            )));
        }
    }
    counts.total = counts.passed + counts.failed;
    Ok(counts)
}

/// 预验/复跑的实测记录（进 SeedOracleVerified.measured / RedProven.replay，机器可比对）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Measured {
    #[serde(rename = "fileFailed")]
    pub file_failed: usize,
    #[serde(rename = "filePassed")]
    pub file_passed: usize,
    #[serde(rename = "totalFailed")]
    pub total_failed: usize,
    #[serde(rename = "totalPassed")]
    pub total_passed: usize,
    pub total: usize,
    #[serde(rename = "failedCases")]
    pub failed_cases: Vec<String>,
    #[serde(rename = "redForm", skip_serializing_if = "Option::is_none")]
    pub red_form: Option<String>,
}

/// One canonical rustc diagnostic used to bind a compile-red oracle.
///
/// Deserialization is intentionally stricter than ordinary archived payload
/// parsing: v2 is a high-water mark, so malformed, unsorted, duplicated, or
/// non-canonical identities must fail closed instead of regaining legacy
/// counts-only semantics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "RawCompileDiagnostic")]
pub struct CompileDiagnostic {
    pub code: String,
    pub keys: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompileDiagnostic {
    code: String,
    keys: Vec<String>,
}

impl TryFrom<RawCompileDiagnostic> for CompileDiagnostic {
    type Error = String;

    fn try_from(raw: RawCompileDiagnostic) -> Result<Self, Self::Error> {
        if !canonical_rust_code(&raw.code) {
            return Err(format!("non-canonical rustc error code: {:?}", raw.code));
        }
        if raw.keys.is_empty() {
            return Err(format!("{} diagnostic has no identity keys", raw.code));
        }
        if !strictly_sorted(&raw.keys) {
            return Err(format!(
                "{} diagnostic keys are empty, duplicated, or unsorted",
                raw.code
            ));
        }
        for key in &raw.keys {
            validate_compile_key(key)?;
        }
        Ok(Self {
            code: raw.code,
            keys: raw.keys,
        })
    }
}

/// Canonical identity for a set of coded rustc diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "RawCompileRedIdentity")]
pub struct CompileRedIdentity {
    pub dialect: String,
    pub diagnostics: Vec<CompileDiagnostic>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompileRedIdentity {
    dialect: String,
    diagnostics: Vec<CompileDiagnostic>,
}

impl TryFrom<RawCompileRedIdentity> for CompileRedIdentity {
    type Error = String;

    fn try_from(raw: RawCompileRedIdentity) -> Result<Self, Self::Error> {
        if raw.dialect != "rustc" {
            return Err(format!(
                "compile identity dialect must be rustc, got {:?}",
                raw.dialect
            ));
        }
        if raw.diagnostics.is_empty() {
            return Err("compile identity has no coded rustc diagnostics".into());
        }
        if !strictly_sorted(&raw.diagnostics) {
            return Err("compile diagnostics are duplicated or unsorted".into());
        }
        Ok(Self {
            dialect: raw.dialect,
            diagnostics: raw.diagnostics,
        })
    }
}

/// Machine-recomputed proof that a human `--expected-red` assertion was
/// satisfied by the measured observation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "form", rename_all = "lowercase", deny_unknown_fields)]
pub enum ExpectedRedProof {
    Compile {
        #[serde(rename = "rustCodes")]
        rust_codes: Vec<String>,
        #[serde(rename = "rustKeys")]
        rust_keys: Vec<String>,
    },
    Assertion {
        #[serde(rename = "failedCount")]
        failed_count: usize,
    },
}

/// Task-scoped compile baseline strength used by collect replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileOracleMatch {
    NoBaseline,
    LegacyCountsOnly,
    IdentityV2,
}

/// Classification of a compile-red symbol identity change between oracle
/// preverification and collect replay.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileIdentityShift {
    Unchanged,
    LegitimateShrink { dropped: Vec<String> },
    Mismatch { reason: String },
}

/// Classify a compile-red symbol-set change without consulting mutable state.
///
/// The ordering of these checks is part of the contract: an unchanged empty
/// set is unchanged, growth always wins over shrink analysis, and a replay
/// that has become empty can never be explained away by upstream work.
#[doc(hidden)]
pub fn classify_compile_identity_shift(
    baseline_symbols: &[String],
    replay_symbols: &[String],
    upstream_provided_symbols: &[String],
) -> CompileIdentityShift {
    let baseline = baseline_symbols.iter().cloned().collect::<BTreeSet<_>>();
    let replay = replay_symbols.iter().cloned().collect::<BTreeSet<_>>();
    let upstream = upstream_provided_symbols
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();

    if baseline == replay {
        return CompileIdentityShift::Unchanged;
    }

    let added = replay.difference(&baseline).cloned().collect::<Vec<_>>();
    if !added.is_empty() {
        return CompileIdentityShift::Mismatch {
            reason: format!("compile identity grew by symbols {added:?}"),
        };
    }

    if replay.is_empty() {
        return CompileIdentityShift::Mismatch {
            reason: "compile identity replay is empty".to_string(),
        };
    }

    let dropped = baseline.difference(&replay).cloned().collect::<Vec<_>>();
    let unexplained = dropped
        .iter()
        .filter(|symbol| !upstream.contains(*symbol))
        .cloned()
        .collect::<Vec<_>>();
    if unexplained.is_empty() {
        CompileIdentityShift::LegitimateShrink { dropped }
    } else {
        CompileIdentityShift::Mismatch {
            reason: format!("compile identity shrink has unexplained symbols {unexplained:?}"),
        }
    }
}

/// Additive observation wrapper. `Measured` remains source-compatible for
/// archived struct literals, while schema-v2 JSON gains `compileIdentity`
/// beside the legacy measured fields through `flatten`. A live preverify may also hold an internal
/// one-shot gate observation; it is never serialized and is published only after expected-red proof.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OracleObservation {
    #[serde(flatten)]
    pub measured: Measured,
    #[serde(
        rename = "compileIdentity",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub compile_identity: Option<CompileRedIdentity>,
    #[serde(skip)]
    pending_gate_execution: Option<Arc<Mutex<Option<PendingGateExecution>>>>,
}

struct PendingGateExecution {
    root: PathBuf,
    round: String,
    task_id: String,
    gate_run_id: String,
    subject_tree_sha: String,
    result: gate::GateResult,
    fingerprint: gate::GateEnvironmentFingerprint,
}

impl std::fmt::Debug for PendingGateExecution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingGateExecution")
            .field("round", &self.round)
            .field("task_id", &self.task_id)
            .field("gate_run_id", &self.gate_run_id)
            .field("gate", &self.result.name)
            .finish_non_exhaustive()
    }
}

impl PendingGateExecution {
    fn record(&self) -> Result<()> {
        gate::record_gate_execution(
            &self.root,
            &self.round,
            crate::ledger::GateAuditIdentity::PreAttempt {
                task_id: &self.task_id,
            },
            GATE_EXECUTED_SCHEMA,
            gate::GatePhase::RedReplay,
            &self.gate_run_id,
            &self.subject_tree_sha,
            &self.result,
            &self.fingerprint,
        )
    }
}

impl OracleObservation {
    fn record_pending_gate_execution(&self) -> Result<()> {
        let Some(pending) = &self.pending_gate_execution else {
            return Ok(());
        };
        let mut pending = pending
            .lock()
            .map_err(|_| anyhow::anyhow!("oracle pending gate observation lock poisoned"))?;
        let Some(execution) = pending.as_ref() else {
            return Ok(());
        };
        execution.record()?;
        *pending = None;
        Ok(())
    }
}

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn canonical_rust_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 5 && bytes[0] == b'E' && bytes[1..].iter().all(u8::is_ascii_digit)
}

fn rust_symbol(token: &str) -> bool {
    if token.is_empty() || token.contains('/') || token.contains('\\') {
        return false;
    }
    token.split("::").all(|segment| {
        let segment = segment.strip_prefix("r#").unwrap_or(segment);
        !segment.is_empty()
            && segment
                .chars()
                .next()
                .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            && segment
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

fn normalize_message(message: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut normalized = String::new();
    let mut number_run = false;
    for raw_word in collapsed.split(' ') {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        let path_like = raw_word.contains('/')
            || raw_word.contains('\\')
            || raw_word.to_ascii_lowercase().contains(".rs:");
        if path_like {
            normalized.push_str("<path>");
            number_run = false;
            continue;
        }
        for character in raw_word.chars() {
            if character.is_ascii_digit() {
                if !number_run {
                    normalized.push_str("<n>");
                    number_run = true;
                }
            } else {
                normalized.push(character);
                number_run = false;
            }
        }
        number_run = false;
    }
    normalized
}

fn validate_compile_key(key: &str) -> Result<(), String> {
    if let Some(symbol) = key.strip_prefix("symbol:") {
        if rust_symbol(symbol) {
            return Ok(());
        }
        return Err(format!("non-canonical symbol key: {key:?}"));
    }
    if let Some(message) = key.strip_prefix("message:") {
        if !message.is_empty() && normalize_message(message) == message {
            return Ok(());
        }
        return Err(format!("non-canonical message key: {key:?}"));
    }
    Err(format!("unknown compile identity key: {key:?}"))
}

fn strip_ansi_csi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' || chars.peek() != Some(&'[') {
            output.push(character);
            continue;
        }
        chars.next();
        for next in chars.by_ref() {
            if ('@'..='~').contains(&next) {
                break;
            }
        }
    }
    output
}

fn backtick_symbols(text: &str) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            break;
        };
        let token = &after_open[..close];
        if rust_symbol(token) {
            symbols.insert(format!("symbol:{token}"));
        }
        rest = &after_open[close + 1..];
    }
    symbols
}

fn parse_rustc_headline(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("error[")?;
    let close = rest.find("]:")?;
    let raw_code = &rest[..close];
    if raw_code.len() != 5
        || !matches!(raw_code.as_bytes()[0], b'E' | b'e')
        || !raw_code.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let code = format!("E{}", &raw_code[1..]);
    let headline = rest[close + 2..].trim().to_string();
    Some((code, headline))
}

/// Parse and canonicalize coded rustc error headlines.
pub fn canonical_rust_compile_identity(log: &str) -> Result<CompileRedIdentity, String> {
    let clean = strip_ansi_csi(log);
    let mut diagnostics = BTreeSet::new();
    for line in clean.lines() {
        let Some((code, headline)) = parse_rustc_headline(line) else {
            continue;
        };
        let symbols = backtick_symbols(&headline);
        let keys = if symbols.is_empty() {
            let message = normalize_message(&headline);
            if message.is_empty() {
                return Err(format!("{code} rustc diagnostic has an empty headline"));
            }
            vec![format!("message:{message}")]
        } else {
            symbols.into_iter().collect()
        };
        diagnostics.insert(CompileDiagnostic { code, keys });
    }
    if diagnostics.is_empty() {
        return Err("no coded rustc error[Edddd] diagnostics found".into());
    }
    Ok(CompileRedIdentity {
        dialect: "rustc".into(),
        diagnostics: diagnostics.into_iter().collect(),
    })
}

fn expected_compile_parts(expected: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let clean = strip_ansi_csi(expected);
    let bytes = clean.as_bytes();
    let mut codes = BTreeSet::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let Some(relative) = clean[offset..].find("error[") else {
            break;
        };
        let start = offset + relative + "error[".len();
        if start + 6 <= bytes.len()
            && matches!(bytes[start], b'E' | b'e')
            && bytes[start + 1..start + 5].iter().all(u8::is_ascii_digit)
            && bytes[start + 5] == b']'
        {
            codes.insert(format!("E{}", &clean[start + 1..start + 5]));
            offset = start + 6;
        } else {
            offset = start;
        }
    }
    if codes.is_empty() {
        return Err("--expected-red compile assertion must contain error[Edddd]".into());
    }
    Ok((
        codes.into_iter().collect(),
        backtick_symbols(&clean).into_iter().collect(),
    ))
}

/// Validate a compile expectation against the complete canonical identity.
pub fn validate_expected_compile_red(
    expected: &str,
    actual: &CompileRedIdentity,
) -> Result<ExpectedRedProof, String> {
    let (rust_codes, rust_keys) = expected_compile_parts(expected)?;
    let actual_codes = actual
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect::<BTreeSet<_>>();
    let actual_keys = actual
        .diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.keys.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for code in &rust_codes {
        if !actual_codes.contains(code.as_str()) {
            return Err(format!(
                "expected rustc code {code} is absent from measured compile identity"
            ));
        }
    }
    for key in &rust_keys {
        if !actual_keys.contains(key.as_str()) {
            return Err(format!(
                "expected rustc key {key} is absent from measured compile identity"
            ));
        }
    }
    Ok(ExpectedRedProof::Compile {
        rust_codes,
        rust_keys,
    })
}

fn expected_assertion_count(expected: &str) -> Result<usize, String> {
    let runs = expected
        .split(|character: char| !character.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .collect::<Vec<_>>();
    if runs.len() != 1 {
        return Err(
            "--expected-red assertion must contain exactly one unambiguous positive integer".into(),
        );
    }
    let count = runs[0]
        .parse::<usize>()
        .map_err(|error| format!("invalid assertion failed-count: {error}"))?;
    if count == 0 {
        return Err("--expected-red assertion failed-count must be positive".into());
    }
    Ok(count)
}

fn validate_expected_assertion_red(
    expected: &str,
    measured: &Measured,
) -> Result<ExpectedRedProof, String> {
    let failed_count = expected_assertion_count(expected)?;
    if measured.red_form.as_deref() == Some("compile") {
        return Err("card redForm assertion cannot accept a compile-red observation".into());
    }
    if failed_count != measured.file_failed {
        return Err(format!(
            "expected assertion failed-count {failed_count} != measured fileFailed {}",
            measured.file_failed
        ));
    }
    Ok(ExpectedRedProof::Assertion { failed_count })
}

pub(crate) fn validate_expected_red_syntax(
    red_form: Option<&str>,
    dialect: &str,
    expected: &str,
    record_only: bool,
) -> Result<(), String> {
    if record_only {
        return Err(
            "schema-v2 SeedOracleVerified rejects --record-only because no measured proof exists"
                .into(),
        );
    }
    match red_form {
        Some("compile") => {
            if dialect != "cargo" {
                return Err(format!(
                    "card redForm compile requires cargo oracle dialect, got {dialect:?}"
                ));
            }
            expected_compile_parts(expected)?;
            Ok(())
        }
        Some("assertion") => {
            expected_assertion_count(expected)?;
            Ok(())
        }
        Some(other) => Err(format!(
            "card redForm must be the closed compile|assertion enum, got {other:?}"
        )),
        None => Err("card redForm is required for schema-v2 SeedOracleVerified".into()),
    }
}

/// Prove the card's expected-red claim and only then publish a pending preverify observation.
///
/// Syntax, O5 localization, and identity mismatches therefore remain zero-ledger rejections, while
/// a successful proof still emits the same canonical `GateExecuted` fact before its caller records
/// `SeedOracleVerified`.
pub(crate) fn prove_expected_red(
    red_form: &str,
    expected: &str,
    observation: &OracleObservation,
) -> Result<ExpectedRedProof, String> {
    let proof = match red_form {
        "compile" => {
            if observation.measured.red_form.as_deref() != Some("compile") {
                return Err(format!(
                    "card redForm compile != measured redForm {:?}",
                    observation.measured.red_form
                ));
            }
            let identity = observation
                .compile_identity
                .as_ref()
                .ok_or_else(|| "compile-red observation lacks compileIdentity".to_string())?;
            validate_expected_compile_red(expected, identity)
        }
        "assertion" => {
            if observation.measured.red_form.as_deref() != Some("assertion") {
                return Err(format!(
                    "card redForm assertion != measured redForm {:?}",
                    observation.measured.red_form
                ));
            }
            if observation.compile_identity.is_some() {
                return Err(
                    "assertion-red observation unexpectedly contains compileIdentity".into(),
                );
            }
            validate_expected_assertion_red(expected, &observation.measured)
        }
        other => Err(format!("unknown card redForm {other:?}")),
    }?;
    observation
        .record_pending_gate_execution()
        .map_err(|error| format!("persisting preverify GateExecuted failed: {error:#}"))?;
    Ok(proof)
}

fn v2_compile_identity(payload: &serde_json::Value) -> Result<CompileRedIdentity, String> {
    let expected = payload
        .get("expectedRed")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "schema-v2 oracle payload lacks string expectedRed".to_string())?;
    let measured = payload
        .get("measured")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "schema-v2 oracle payload lacks measured object".to_string())?;
    if measured.get("redForm").and_then(serde_json::Value::as_str) != Some("compile") {
        return Err("schema-v2 compile baseline measured.redForm is not compile".into());
    }
    let identity_value = measured
        .get("compileIdentity")
        .cloned()
        .ok_or_else(|| "schema-v2 compile baseline lacks compileIdentity".to_string())?;
    let identity = serde_json::from_value::<CompileRedIdentity>(identity_value)
        .map_err(|error| format!("malformed schema-v2 compileIdentity: {error}"))?;
    let proof_value = payload
        .get("expectedRedProof")
        .cloned()
        .ok_or_else(|| "schema-v2 oracle payload lacks expectedRedProof".to_string())?;
    let proof = serde_json::from_value::<ExpectedRedProof>(proof_value)
        .map_err(|error| format!("malformed schema-v2 expectedRedProof: {error}"))?;
    let recomputed = validate_expected_compile_red(expected, &identity)?;
    if proof != recomputed {
        return Err(format!(
            "schema-v2 expectedRedProof is not self-consistent: stored={proof:?} recomputed={recomputed:?}"
        ));
    }
    Ok(identity)
}

fn compile_oracle_baseline_state(
    task_payloads: &[serde_json::Value],
) -> Result<(CompileOracleMatch, Option<CompileRedIdentity>), String> {
    let mut crossed_v2 = false;
    let mut latest_v2 = None;
    for payload in task_payloads {
        match payload.get("oracleSchemaVersion") {
            None => {
                if crossed_v2 {
                    return Err(
                        "oracle schema downgrade: versionless payload follows schema v2".into(),
                    );
                }
            }
            Some(version) => {
                let version = version.as_u64().ok_or_else(|| {
                    "oracleSchemaVersion must be an integer schema version".to_string()
                })?;
                if version != 2 {
                    return Err(format!("unknown oracleSchemaVersion {version}"));
                }
                crossed_v2 = true;
                latest_v2 = Some(v2_compile_identity(payload)?);
            }
        }
    }

    if crossed_v2 {
        let baseline = latest_v2
            .ok_or_else(|| "schema-v2 high-water has no valid compile baseline".to_string())?;
        return Ok((CompileOracleMatch::IdentityV2, Some(baseline)));
    }

    let Some(latest) = task_payloads.last() else {
        return Ok((CompileOracleMatch::NoBaseline, None));
    };
    let legacy_measured = latest
        .get("measured")
        .and_then(serde_json::Value::as_object)
        .is_some();
    if legacy_measured {
        Ok((CompileOracleMatch::LegacyCountsOnly, None))
    } else {
        Ok((CompileOracleMatch::NoBaseline, None))
    }
}

/// Compare a replay identity with all task-scoped SeedOracleVerified payloads.
///
/// Legacy tasks retain an explicit degraded counts-only state. Once any v2
/// payload appears, every later event must remain a valid v2 payload and the
/// latest canonical identity must exactly equal replay.
pub fn compile_oracle_baseline_match(
    task_payloads: &[serde_json::Value],
    replay: &CompileRedIdentity,
) -> Result<CompileOracleMatch, String> {
    let (matched, baseline) = compile_oracle_baseline_state(task_payloads)?;
    if let Some(baseline) = baseline {
        if &baseline != replay {
            return Err(format!(
                "compile identity mismatch: baseline={baseline:?} replay={replay:?}"
            ));
        }
    }
    Ok(matched)
}

#[derive(Debug)]
struct LegitimateShrinkEvidence {
    baseline_symbols: Vec<String>,
    replay_symbols: Vec<String>,
    dropped: Vec<String>,
    upstream_tasks: Vec<String>,
}

#[derive(Debug)]
struct CompileReplayDecision {
    matched: CompileOracleMatch,
    legitimate_shrink: Option<LegitimateShrinkEvidence>,
}

fn compile_identity_symbols(identity: &CompileRedIdentity) -> Vec<String> {
    identity
        .diagnostics
        .iter()
        .flat_map(|diagnostic| diagnostic.keys.iter())
        .filter(|key| key.starts_with("symbol:"))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn identity_after_dropping_symbols(
    baseline: &CompileRedIdentity,
    dropped: &[String],
    upstream: &[String],
) -> CompileRedIdentity {
    let dropped = dropped.iter().collect::<BTreeSet<_>>();
    let upstream = upstream.iter().cloned().collect::<BTreeSet<_>>();
    let diagnostics = baseline
        .diagnostics
        .iter()
        .filter_map(|diagnostic| {
            let grouped_import_resolved = diagnostic.keys.iter().any(|item| {
                let Some(item_name) = item.strip_prefix("symbol:") else {
                    return false;
                };
                if item_name.contains("::") || !dropped.contains(item) {
                    return false;
                }
                diagnostic.keys.iter().any(|module| {
                    let Some(module_name) = module.strip_prefix("symbol:") else {
                        return false;
                    };
                    module_name != item_name
                        && upstream.contains(&format!("symbol:{module_name}::{item_name}"))
                })
            });
            if grouped_import_resolved {
                return None;
            }
            let keys = diagnostic
                .keys
                .iter()
                .filter(|key| !dropped.contains(key))
                .cloned()
                .collect::<Vec<_>>();
            (!keys.is_empty()).then(|| CompileDiagnostic {
                code: diagnostic.code.clone(),
                keys,
            })
        })
        .collect();
    CompileRedIdentity {
        dialect: baseline.dialect.clone(),
        diagnostics,
    }
}

fn upstream_symbols_with_grouped_import_aliases(
    baseline: &CompileRedIdentity,
    upstream: &[String],
) -> BTreeSet<String> {
    let mut explained = upstream.iter().cloned().collect::<BTreeSet<_>>();
    let authenticated = explained.clone();
    for diagnostic in &baseline.diagnostics {
        for item in &diagnostic.keys {
            let Some(item_name) = item.strip_prefix("symbol:") else {
                continue;
            };
            if item_name.contains("::") {
                continue;
            }
            for module in &diagnostic.keys {
                let Some(module_name) = module.strip_prefix("symbol:") else {
                    continue;
                };
                if module_name == item_name {
                    continue;
                }
                let qualified = format!("symbol:{module_name}::{item_name}");
                if authenticated.contains(&qualified) {
                    explained.insert(item.clone());
                    // rustc reports a grouped import as a bare item plus its
                    // module context. When the authenticated `module::item`
                    // resolves, the whole diagnostic disappears, including
                    // that contextual module key. The complete-identity seam
                    // below still requires that atomic disappearance to
                    // reproduce replay exactly.
                    explained.insert(module.clone());
                }
            }
        }
    }
    explained
}

fn compile_oracle_replay_match(
    task_payloads: &[serde_json::Value],
    effective_rebaseline: Option<&CompileRedIdentity>,
    replay: &CompileRedIdentity,
    upstream_provided_symbols: &[String],
) -> Result<CompileReplayDecision, String> {
    let (matched, seed_baseline) = compile_oracle_baseline_state(task_payloads)?;
    let Some(seed_baseline) = seed_baseline else {
        return Ok(CompileReplayDecision {
            matched,
            legitimate_shrink: None,
        });
    };
    let baseline = effective_rebaseline.unwrap_or(&seed_baseline);
    if baseline == replay {
        return Ok(CompileReplayDecision {
            matched,
            legitimate_shrink: None,
        });
    }

    let baseline_symbols = compile_identity_symbols(baseline);
    let replay_symbols = compile_identity_symbols(replay);
    let upstream_with_aliases =
        upstream_symbols_with_grouped_import_aliases(baseline, upstream_provided_symbols)
            .into_iter()
            .collect::<Vec<_>>();
    if let CompileIdentityShift::LegitimateShrink { dropped } =
        classify_compile_identity_shift(&baseline_symbols, &replay_symbols, &upstream_with_aliases)
    {
        // The symbol classifier is intentionally small and pure. The replay
        // seam additionally proves that removing exactly those symbols from
        // the complete coded identity yields replay, so code/message changes
        // cannot hide behind a symbol-only shrink.
        if identity_after_dropping_symbols(baseline, &dropped, upstream_provided_symbols) == *replay
        {
            return Ok(CompileReplayDecision {
                matched,
                legitimate_shrink: Some(LegitimateShrinkEvidence {
                    baseline_symbols,
                    replay_symbols,
                    dropped,
                    upstream_tasks: Vec::new(),
                }),
            });
        }
    }

    // Preserve the existing hard-failure diagnostic verbatim for every
    // change that is not completely explained by recorded upstream work.
    Err(format!(
        "compile identity mismatch: baseline={baseline:?} replay={replay:?}"
    ))
}

fn canonical_string_array(payload: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    let values = payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("legitimate-shrink evidence lacks array {key}"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("legitimate-shrink evidence {key} contains non-string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !strictly_sorted(&values) {
        return Err(format!(
            "legitimate-shrink evidence {key} is empty, duplicated, or unsorted"
        ));
    }
    Ok(values)
}

const LEGITIMATE_SHRINK_REASON: &str = "compile identity legitimate upstream shrink";

fn legitimate_shrink_rebaseline(
    payload: &serde_json::Value,
) -> Result<Option<CompileRedIdentity>, String> {
    if payload.get("stage").and_then(serde_json::Value::as_str) != Some("red-replay")
        || payload.get("reason").and_then(serde_json::Value::as_str)
            != Some(LEGITIMATE_SHRINK_REASON)
    {
        return Ok(None);
    }
    let baseline_symbols = canonical_string_array(payload, "baselineSymbols")?;
    let replay_symbols = canonical_string_array(payload, "replaySymbols")?;
    let dropped = canonical_string_array(payload, "dropped")?;
    let upstream_tasks = canonical_string_array(payload, "upstreamTasks")?;
    if baseline_symbols.is_empty()
        || replay_symbols.is_empty()
        || dropped.is_empty()
        || upstream_tasks.is_empty()
        || baseline_symbols
            .iter()
            .chain(replay_symbols.iter())
            .chain(dropped.iter())
            .any(|symbol| !symbol.starts_with("symbol:") || validate_compile_key(symbol).is_err())
        || upstream_tasks
            .iter()
            .any(|task_id| card::validate_task_id(task_id).is_err())
    {
        return Err("malformed legitimate-shrink evidence".to_string());
    }
    match classify_compile_identity_shift(&baseline_symbols, &replay_symbols, &dropped) {
        CompileIdentityShift::LegitimateShrink {
            dropped: recomputed,
        } if recomputed == dropped => {}
        other => {
            return Err(format!(
                "legitimate-shrink evidence is inconsistent: {other:?}"
            ));
        }
    }
    let identity_value = payload
        .get("rebaselineCompileIdentity")
        .cloned()
        .ok_or_else(|| {
            "legitimate-shrink escalation lacks rebaselineCompileIdentity".to_string()
        })?;
    let identity = serde_json::from_value::<CompileRedIdentity>(identity_value)
        .map_err(|error| format!("malformed rebaseline compileIdentity: {error}"))?;
    if compile_identity_symbols(&identity) != replay_symbols {
        return Err(
            "legitimate-shrink escalation replaySymbols != rebaselineCompileIdentity".to_string(),
        );
    }
    Ok(Some(identity))
}

/// Return the latest durable legitimate-shrink identity after the latest v2
/// SeedOracleVerified event. A later explicit preverification supersedes an
/// automatic rebaseline; malformed rebaseline evidence fails closed.
fn latest_compile_rebaseline(
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
) -> Result<Option<CompileRedIdentity>, String> {
    let mut v2_seen = false;
    let mut latest = None;
    for event in events.iter().filter(|event| {
        event.task_id.as_deref() == Some(task_id) && event.round.as_deref() == Some(round)
    }) {
        if event.kind == "SeedOracleVerified" {
            if event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("oracleSchemaVersion"))
                .and_then(serde_json::Value::as_u64)
                == Some(2)
            {
                v2_seen = true;
                latest = None;
            }
            continue;
        }
        if event.kind != "EscalationRaised" {
            continue;
        }
        let Some(payload) = event.payload.as_ref() else {
            continue;
        };
        let Some(identity) = legitimate_shrink_rebaseline(payload)? else {
            continue;
        };
        if event.actor != "runtime:orch" {
            return Err("legitimate-shrink escalation has non-canonical actor".to_string());
        }
        if !v2_seen {
            return Err(
                "legitimate-shrink escalation precedes schema-v2 SeedOracleVerified".to_string(),
            );
        }
        latest = Some(identity);
    }
    Ok(latest)
}

fn public_item_name(line: &str) -> Option<String> {
    // rustfmt keeps module-scope declarations at column zero. Restricting the
    // evidence extractor to that canonical form avoids mistaking public impl
    // methods or declarations inside inline modules for crate-module exports.
    let rest = line.strip_prefix("pub ")?;
    let tokens = rest
        .split(|character: char| {
            !(character == '_' || character == '#' || character.is_ascii_alphanumeric())
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let first = *tokens.first()?;
    let name = match first {
        "fn" | "struct" | "enum" | "type" | "trait" | "union" | "mod" | "static" => {
            *tokens.get(1)?
        }
        "const" if tokens.get(1) == Some(&"fn") => *tokens.get(2)?,
        "const" => *tokens.get(1)?,
        "async" | "unsafe" | "extern" => {
            let fn_index = tokens.iter().position(|token| *token == "fn")?;
            *tokens.get(fn_index + 1)?
        }
        // Re-export syntax may span lines or rename imports. Unsupported
        // shapes deliberately contribute no evidence (fail closed).
        _ => return None,
    };
    rust_symbol(name).then(|| name.to_string())
}

fn public_module_prefix(path: &str) -> Option<String> {
    let parts = path.split('/').collect::<Vec<_>>();
    if parts.len() < 5 || parts[..2] != ["orch", "crates"] || parts[3] != "src" {
        return None;
    }
    let crate_name = parts[2].replace('-', "_");
    let source = &parts[4..];
    let filename = *source.last()?;
    if !filename.ends_with(".rs") || filename == "main.rs" || source.first() == Some(&"bin") {
        return None;
    }
    let mut modules = source[..source.len() - 1].to_vec();
    match filename {
        "lib.rs" if modules.is_empty() => {}
        "mod.rs" => {}
        _ => modules.push(filename.strip_suffix(".rs")?),
    }
    let mut prefix = crate_name;
    for module in modules {
        if !rust_symbol(module) {
            return None;
        }
        prefix.push_str("::");
        prefix.push_str(module);
    }
    Some(prefix)
}

fn public_symbols_in_source(path: &str, source: &str) -> BTreeSet<String> {
    let Some(prefix) = public_module_prefix(path) else {
        return BTreeSet::new();
    };
    source
        .lines()
        .filter_map(public_item_name)
        .flat_map(|name| {
            let mut symbols = vec![format!("symbol:{prefix}::{name}")];
            // rustc reports a grouped import such as
            // `use orch_host::harness::{self, registry_digest}` as
            // `symbol:harness::registry_digest`, while an ordinary unresolved
            // path can retain the crate-qualified
            // `symbol:orch_host::harness::registry_digest` shape.  Both refer
            // to the same authenticated public item from the same recorded
            // upstream merge, so retain both canonical diagnostic spellings.
            if let Some((_, crate_stripped)) = prefix.split_once("::") {
                symbols.push(format!("symbol:{crate_stripped}::{name}"));
            }
            symbols
        })
        .collect()
}

fn full_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn introduced_public_symbols(
    root: &Path,
    merge_sha: &str,
    write_set: &[String],
) -> Option<BTreeSet<String>> {
    if !full_git_sha(merge_sha) || !gitx::is_ancestor(root, merge_sha, "main").ok()? {
        return None;
    }
    let first_parent = gitx::rev_parse(root, &format!("{merge_sha}^1")).ok()?;
    // A recorded ordinary delivery is a no-ff merge. Requiring a second
    // parent prevents a forged single-parent commit from defining evidence.
    gitx::rev_parse(root, &format!("{merge_sha}^2")).ok()?;
    let changed = gitx::diff_names(root, &first_parent, merge_sha)
        .ok()?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut before_symbols = BTreeSet::new();
    let mut after_symbols = BTreeSet::new();
    for path in write_set
        .iter()
        .filter(|path| changed.contains(path.as_str()))
        .filter(|path| public_module_prefix(path).is_some())
    {
        let prefix = public_module_prefix(path)?;
        // Only a successful tree lookup with no exact entry proves a new
        // source file.  Every other object/read failure still abstains rather
        // than turning unavailable evidence into forged "new" symbols.
        let existed_before = gitx::tree_path_exists(root, &first_parent, path).ok()?;
        let before = if existed_before {
            String::from_utf8(gitx::show_bytes(root, &first_parent, path).ok()?).ok()?
        } else {
            String::new()
        };
        let after = String::from_utf8(gitx::show_bytes(root, merge_sha, path).ok()?).ok()?;
        before_symbols.extend(public_symbols_in_source(path, &before));
        after_symbols.extend(public_symbols_in_source(path, &after));
        if !existed_before {
            // A newly added source file introduces its module identity as
            // well as its public items.  rustc can diagnose the former as
            // `symbol:<crate>::<module>` without naming an item.
            after_symbols.insert(format!("symbol:{prefix}"));
            if let Some((_, crate_stripped)) = prefix.split_once("::") {
                after_symbols.insert(format!("symbol:{crate_stripped}"));
            }
        }
    }
    Some(after_symbols.difference(&before_symbols).cloned().collect())
}

/// Test-facing wrapper around the fail-closed upstream symbol extractor.
#[doc(hidden)]
pub fn introduced_public_symbols_probe(
    root: &Path,
    merge_sha: &str,
    write_set: &[String],
) -> Option<BTreeSet<String>> {
    introduced_public_symbols(root, merge_sha, write_set)
}

#[derive(Default)]
struct UpstreamSymbolEvidence {
    by_task: BTreeMap<String, BTreeSet<String>>,
}

impl UpstreamSymbolEvidence {
    fn symbols(&self) -> Vec<String> {
        self.by_task
            .values()
            .flat_map(|symbols| symbols.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn tasks_explaining(&self, dropped: &[String], baseline: &CompileRedIdentity) -> Vec<String> {
        let dropped = dropped.iter().collect::<BTreeSet<_>>();
        self.by_task
            .iter()
            .filter(|(_, symbols)| {
                let symbols = symbols.iter().cloned().collect::<Vec<_>>();
                upstream_symbols_with_grouped_import_aliases(baseline, &symbols)
                    .iter()
                    .any(|symbol| dropped.contains(symbol))
            })
            .map(|(task, _)| task.clone())
            .collect()
    }
}

fn recorded_upstream_public_symbols(
    root: &Path,
    round: &str,
    c: &card::Card,
    events: &[orch_core::EventRecord],
    ledger_clean: bool,
) -> UpstreamSymbolEvidence {
    // A direct dependency carries its already-recorded ancestors into the
    // candidate baseline. Walk the signed card graph transitively, but admit
    // symbols only after every visited node proves its unique all-green
    // TaskRecorded and preceding no-ff MergeExecuted pair.
    fn collect_recorded_task(
        root: &Path,
        round: &str,
        upstream_task: &str,
        events: &[orch_core::EventRecord],
        visiting: &mut BTreeSet<String>,
        completed: &mut BTreeSet<String>,
        by_task: &mut BTreeMap<String, BTreeSet<String>>,
    ) -> Option<()> {
        if completed.contains(upstream_task) {
            return Some(());
        }
        if !visiting.insert(upstream_task.to_string()) {
            return None;
        }

        let upstream_card = card::load(root, round, upstream_task).ok()?;
        for dependency in &upstream_card.meta.depends_on {
            collect_recorded_task(
                root, round, dependency, events, visiting, completed, by_task,
            )?;
        }
        let recorded = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "TaskRecorded"
                    && event.task_id.as_deref() == Some(upstream_task)
                    && event.round.as_deref() == Some(round)
                    && event.actor == "runtime:orch"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("postMergeGates"))
                        .and_then(serde_json::Value::as_str)
                        == Some("all-green")
            })
            .collect::<Vec<_>>();
        if recorded.len() != 1 {
            return None;
        }
        let recorded_index = recorded[0].0;
        let merged = events
            .iter()
            .enumerate()
            .filter(|(index, event)| {
                *index < recorded_index
                    && event.kind == "MergeExecuted"
                    && event.task_id.as_deref() == Some(upstream_task)
                    && event.round.as_deref() == Some(round)
                    && event.actor == "reviewer:orch-runtime"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("policy"))
                        .and_then(serde_json::Value::as_str)
                        == Some("no-ff")
            })
            .collect::<Vec<_>>();
        if merged.len() != 1 {
            return None;
        }
        let merge_sha = merged[0].1.payload.as_ref()?.get("mergeSha")?.as_str()?;
        let symbols = introduced_public_symbols(root, merge_sha, &upstream_card.meta.write_set)?;
        by_task.insert(upstream_task.to_string(), symbols);
        visiting.remove(upstream_task);
        completed.insert(upstream_task.to_string());
        Some(())
    }

    let collected = (|| -> Option<UpstreamSymbolEvidence> {
        if !ledger_clean {
            return None;
        }
        let mut by_task = BTreeMap::new();
        let mut visiting = BTreeSet::new();
        let mut completed = BTreeSet::new();
        for upstream_task in &c.meta.depends_on {
            collect_recorded_task(
                root,
                round,
                upstream_task,
                events,
                &mut visiting,
                &mut completed,
                &mut by_task,
            )?;
        }
        Some(UpstreamSymbolEvidence { by_task })
    })();
    collected.unwrap_or_default()
}

fn v2_assertion_payload(payload: &serde_json::Value) -> Result<(), String> {
    let expected = payload
        .get("expectedRed")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "schema-v2 assertion payload lacks string expectedRed".to_string())?;
    let measured_value = payload
        .get("measured")
        .cloned()
        .ok_or_else(|| "schema-v2 assertion payload lacks measured".to_string())?;
    if measured_value.get("compileIdentity").is_some() {
        return Err("schema-v2 assertion payload must not contain compileIdentity".into());
    }
    let measured = serde_json::from_value::<Measured>(measured_value)
        .map_err(|error| format!("malformed schema-v2 assertion measured payload: {error}"))?;
    if measured.red_form.as_deref() != Some("assertion") {
        return Err("schema-v2 assertion measured.redForm is not assertion".into());
    }
    let proof_value = payload
        .get("expectedRedProof")
        .cloned()
        .ok_or_else(|| "schema-v2 assertion payload lacks expectedRedProof".to_string())?;
    let proof = serde_json::from_value::<ExpectedRedProof>(proof_value)
        .map_err(|error| format!("malformed schema-v2 assertion expectedRedProof: {error}"))?;
    let recomputed = validate_expected_assertion_red(expected, &measured)?;
    if proof != recomputed {
        return Err(format!(
            "schema-v2 assertion expectedRedProof is not self-consistent: stored={proof:?} recomputed={recomputed:?}"
        ));
    }
    Ok(())
}

fn assertion_oracle_baseline_match(
    task_payloads: &[serde_json::Value],
) -> Result<CompileOracleMatch, String> {
    let mut crossed_v2 = false;
    for payload in task_payloads {
        match payload.get("oracleSchemaVersion") {
            None => {
                if crossed_v2 {
                    return Err(
                        "oracle schema downgrade: versionless payload follows schema v2".into(),
                    );
                }
            }
            Some(version) => {
                let version = version.as_u64().ok_or_else(|| {
                    "oracleSchemaVersion must be an integer schema version".to_string()
                })?;
                if version != 2 {
                    return Err(format!("unknown oracleSchemaVersion {version}"));
                }
                crossed_v2 = true;
                v2_assertion_payload(payload)?;
            }
        }
    }
    if crossed_v2 {
        return Ok(CompileOracleMatch::IdentityV2);
    }
    if task_payloads
        .last()
        .and_then(|payload| payload.get("measured"))
        .and_then(serde_json::Value::as_object)
        .is_some()
    {
        Ok(CompileOracleMatch::LegacyCountsOnly)
    } else {
        Ok(CompileOracleMatch::NoBaseline)
    }
}

/// 核验 REPORT §3 的断言红计数：声称值必须与种子文件自身的实测计数一致。
///
/// 全工作区聚合计数仅用于诊断与基线复现，不能替代文件内计数；编译红没有可比的
/// `test result:` 数字，因而直接豁免数字比较。
pub fn report_claim_ok(claim: &SuiteCounts, measured: &Measured) -> Result<(), String> {
    if measured.red_form.as_deref() == Some("compile") {
        return Ok(());
    }
    if claim.failed == measured.file_failed && claim.passed == measured.file_passed {
        Ok(())
    } else {
        Err(format!(
            "REPORT §3 声称 {}f/{}p ≠ seed commit 文件内实测 {}f/{}p（报数造假，E12 机拦）",
            claim.failed, claim.passed, measured.file_failed, measured.file_passed
        ))
    }
}

/// 先红复跑命令卡驱动化（B32）：取卡 gates.fast **首个**门名（卡内顺序即优先级）。
/// 空列表 → Err（文案含 `seeded` 字样——seeded 任务必须至少一个快门，不许静默回退默认名）。
pub fn red_replay_gate(fast: &[String]) -> Result<String, String> {
    fast.first()
        .cloned()
        .ok_or_else(|| "seeded 任务卡 gates.fast 为空——至少需要一个快门用于先红复跑".to_string())
}

fn first_coded_rustc_error(log: &str) -> Option<String> {
    let line = strip_ansi_csi(log)
        .lines()
        .find(|line| parse_rustc_headline(line).is_some())?
        .to_string();
    Some(line.chars().take(120).collect())
}

fn measure_cargo_observation(
    log: &str,
    exit_code: i32,
    seed_sources: &[String],
    seed_targets: &[String],
) -> Result<OracleObservation> {
    let failed_cases = parse_cargo_failed_cases(log);
    let file_counts = seed_sources
        .iter()
        .map(|source| cargo_file_counts(source, &failed_cases))
        .collect::<Vec<_>>();
    let file_tests = file_counts.iter().map(|counts| counts.tests).sum::<usize>();

    if parse_cargo_suite_counts(log).is_none() && exit_code != 0 {
        let compile_identity = canonical_rust_compile_identity(log)
            .map_err(anyhow::Error::msg)
            .context("cargo compile-red 缺 canonical coded rustc identity")?;
        let error =
            first_coded_rustc_error(log).context("cargo 编译红缺首条 coded rustc error 行")?;
        return Ok(OracleObservation {
            measured: Measured {
                file_failed: file_tests,
                file_passed: 0,
                total_failed: file_tests,
                total_passed: 0,
                total: file_tests,
                failed_cases: vec![format!("<compile-red>: {error}")],
                red_form: Some("compile".into()),
            },
            compile_identity: Some(compile_identity),
            pending_gate_execution: None,
        });
    }

    if seed_sources.len() != seed_targets.len() {
        bail!("seed-not-attributable: seed target/source snapshot lengths differ");
    }
    let specs = seed_targets
        .iter()
        .zip(seed_sources)
        .map(|(target, source)| CargoSeedRunSpec {
            target: target.clone(),
            test_names: cargo_test_names(source),
        })
        .collect::<Vec<_>>();
    let observed = observed_cargo_seed_counts(log, &specs).map_err(anyhow::Error::msg)?;
    let suite =
        parse_cargo_suite_counts(log).context("无法从门日志解析 cargo test result 汇总行")?;
    Ok(OracleObservation {
        measured: Measured {
            file_failed: observed.failed,
            file_passed: observed.passed,
            total_failed: suite.failed,
            total_passed: suite.passed,
            total: suite.total,
            failed_cases,
            red_form: (exit_code != 0 && suite.failed > 0).then(|| "assertion".into()),
        },
        compile_identity: None,
        pending_gate_execution: None,
    })
}

#[cfg(test)]
fn measure_cargo(log: &str, exit_code: i32, seed_sources: &[String]) -> Result<Measured> {
    let targets = seed_sources
        .iter()
        .enumerate()
        .map(|(index, _)| format!("orch/crates/orch-host/tests/seed_{index}.rs"))
        .collect::<Vec<_>>();
    measure_cargo_observation(log, exit_code, seed_sources, &targets)
        .map(|observation| observation.measured)
}

fn validate_cargo_seed_target_owners(workdir: &Path, targets: &[String]) -> Result<()> {
    // Two packages can print the same tests/name.rs and binary basename. A
    // workspace replay must reject that ambiguity instead of adopting the
    // first package's outcomes for a later package that never executed.
    for target in targets {
        let name = Path::new(target)
            .file_name()
            .context("seed target lacks filename")?;
        let expected = workdir.join(target);
        let mut owners = Vec::new();
        for entry in fs::read_dir(workdir.join("orch/crates"))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if !kind.is_dir() && !kind.is_symlink() {
                continue;
            }
            let candidate = entry.path().join("tests").join(name);
            if candidate.try_exists()? {
                owners.push(candidate);
            }
        }
        if owners.len() != 1 || owners[0] != expected {
            bail!(
                "seed-not-attributable: {target} has ambiguous workspace source owners: {owners:?}"
            );
        }
    }
    Ok(())
}

fn parse_test_gate_observation(
    b: &binding::Binding,
    workdir: &Path,
    seeds: &[card::SeedSpec],
    g: &gate::GateResult,
) -> Result<OracleObservation> {
    let log = fs::read_to_string(&g.log_path)?;
    match b.oracle.dialect.as_str() {
        "cargo" => {
            let seed_sources = seeds
                .iter()
                .map(|seed| {
                    fs::read_to_string(workdir.join(&seed.target)).with_context(|| {
                        format!("读取 cargo 种子源码失败（{}）: {}", seed.target, g.log_path)
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let seed_targets = seeds
                .iter()
                .map(|seed| seed.target.clone())
                .collect::<Vec<_>>();
            let observation =
                measure_cargo_observation(&log, g.exit_code, &seed_sources, &seed_targets)
                    .with_context(|| format!("无法解析 cargo 门日志: {}", g.log_path))?;
            if observation.measured.red_form.as_deref() != Some("compile") {
                validate_cargo_seed_target_owners(workdir, &seed_targets)
                    .with_context(|| format!("无法归属 cargo 门日志: {}", g.log_path))?;
            }
            Ok(observation)
        }
        "vitest" => {
            let suite = parse_suite_counts(&log)
                .with_context(|| format!("无法从门日志解析 vitest 汇总行: {}", g.log_path))?;
            let (mut ft, mut ff) = (0usize, 0usize);
            for s in seeds {
                let fc = parse_file_counts(&log, &s.target).with_context(|| {
                    format!("门日志无种子文件行（{}）: {}", s.target, g.log_path)
                })?;
                ft += fc.tests;
                ff += fc.failed;
            }
            Ok(OracleObservation {
                measured: Measured {
                    file_failed: ff,
                    file_passed: ft - ff,
                    total_failed: suite.failed,
                    total_passed: suite.passed,
                    total: suite.total,
                    failed_cases: parse_failed_cases(&log),
                    // Historical vitest observations omitted redForm. The
                    // schema-v2 round boundary adds the card-declared form
                    // without changing legacy preverify JSON.
                    red_form: None,
                },
                compile_identity: None,
                pending_gate_execution: None,
            })
        }
        dialect => bail!("不支持的 oracle dialect: {dialect}"),
    }
}

fn run_test_gate_and_parse_observation_guarded(
    root: &Path,
    round: &str,
    task: &str,
    b: &binding::Binding,
    workdir: &Path,
    seeds: &[card::SeedSpec],
    gate_ref: &str,
) -> Result<OracleObservation> {
    let spec = b
        .commands
        .get(gate_ref)
        .with_context(|| format!("绑定缺命令: {gate_ref}（卡 gates.fast[0] 解析）"))?;
    let log_dir = root.join("coordination/runtime/logs");
    let gate_run_id = ulid::Ulid::new().to_string();
    let scoped_tag = gate::phase_scoped_log_tag(
        round,
        task,
        "pre-attempt",
        gate::GatePhase::RedReplay,
        &gate_run_id,
    );
    let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
    let subject_tree_sha = gate::capture_gate_subject_tree(root, workdir)?;
    // Expected red is a normal non-zero exit; only admission/spawn/timeout is an execution error.
    let g = gate::run_gate_with_audit_identity(
        root,
        round,
        crate::ledger::GateAuditIdentity::PreAttempt { task_id: task },
        gate_ref,
        spec,
        workdir,
        &log_dir,
        &scoped_tag,
    )?;
    let mut observation = parse_test_gate_observation(b, workdir, seeds, &g)?;
    observation.pending_gate_execution = Some(Arc::new(Mutex::new(Some(PendingGateExecution {
        root: root.to_path_buf(),
        round: round.to_string(),
        task_id: task.to_string(),
        gate_run_id,
        subject_tree_sha,
        result: g,
        fingerprint,
    }))));
    Ok(observation)
}

#[allow(clippy::too_many_arguments)]
fn run_test_gate_and_parse_observation_with_permit(
    root: &Path,
    round: &str,
    identity: crate::ledger::GateAuditIdentity<'_>,
    permit: &crate::storage::StoragePermit,
    b: &binding::Binding,
    workdir: &Path,
    gate_run_id: &str,
    scoped_tag: &str,
    seeds: &[card::SeedSpec],
    gate_ref: &str,
) -> Result<OracleObservation> {
    let spec = b
        .commands
        .get(gate_ref)
        .with_context(|| format!("绑定缺命令: {gate_ref}（卡 gates.fast[0] 解析）"))?;
    let log_dir = root.join("coordination/runtime/logs");
    crate::storage::refresh_gate_permit(
        root,
        round,
        identity,
        permit,
        &[
            workdir.to_path_buf(),
            log_dir.clone(),
            root.join("orch/target"),
        ],
    )?;
    let expected_tag = gate::phase_scoped_log_tag(
        round,
        identity.task_id(),
        identity.attempt_id().unwrap_or("pre-attempt"),
        gate::GatePhase::RedReplay,
        gate_run_id,
    );
    if scoped_tag != expected_tag {
        bail!("red replay gate tag 未绑定 typed round/task/attempt/run identity");
    }
    let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
    let subject_tree_sha = gate::capture_gate_subject_tree(root, workdir)?;
    let g = gate::run_gate_with_permit_and_identity(
        permit, identity, gate_ref, spec, workdir, &log_dir, scoped_tag,
    )?;
    gate::record_gate_execution(
        root,
        round,
        identity,
        GATE_EXECUTED_SCHEMA,
        gate::GatePhase::RedReplay,
        gate_run_id,
        &subject_tree_sha,
        &g,
        &fingerprint,
    )?;
    parse_test_gate_observation(b, workdir, seeds, &g)
}

/// Oracle preverification with the schema-v2 observation retained. The public
/// `preverify` compatibility entry point below projects `Measured` from this
/// single gate run.
fn oracle_git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .with_context(|| format!("oracle git {args:?} 启动失败"))?;
    if !output.status.success() {
        bail!(
            "oracle git {args:?} 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

fn oracle_git_text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(oracle_git(root, args)?)
        .context("oracle git stdout 非 UTF-8")?
        .trim()
        .to_string())
}

fn remove_oracle_sandbox(root: &Path, sandbox: &Path) -> Result<()> {
    let expected_parent = root.join(".worktrees");
    if sandbox.parent() != Some(expected_parent.as_path()) {
        bail!("oracle sandbox 必须直属 .worktrees: {}", sandbox.display());
    }
    let metadata = match fs::symlink_metadata(sandbox) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 oracle sandbox 失败: {}", sandbox.display()))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("oracle sandbox 必须是普通目录: {}", sandbox.display());
    }
    match fs::symlink_metadata(sandbox.join(".git")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(metadata) if metadata.is_dir() => {}
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("oracle sandbox .git 不得是 symlink: {}", sandbox.display())
        }
        Ok(_) => bail!("oracle sandbox .git 类型非法: {}", sandbox.display()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 oracle sandbox .git 失败: {}", sandbox.display()))
        }
    }
    crate::util::remove_dir_all_with_enotempty_retry(sandbox)
        .with_context(|| format!("删除 oracle clone 失败: {}", sandbox.display()))?;
    Ok(())
}

fn provision_oracle_clone(root: &Path, sandbox: &Path, base: &str) -> Result<()> {
    match fs::symlink_metadata(sandbox) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!(
            "unique oracle sandbox 已存在，拒绝覆盖: {}",
            sandbox.display()
        ),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 oracle sandbox 失败: {}", sandbox.display()))
        }
    }
    fs::create_dir_all(
        sandbox
            .parent()
            .context("oracle sandbox path 必须有 parent")?,
    )?;
    let setup = || -> Result<()> {
        let output = Command::new("git")
            .arg("-c")
            .arg("protocol.file.allow=always")
            .arg("clone")
            .arg("--quiet")
            .arg("--shared")
            .arg("--no-checkout")
            .arg("--single-branch")
            .arg("--branch")
            .arg("main")
            .arg(root)
            .arg(sandbox)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .context("创建 oracle isolated clone 失败")?;
        if !output.status.success() {
            bail!(
                "创建 oracle isolated clone 失败({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        oracle_git(sandbox, &["checkout", "--quiet", "main"])?;
        if oracle_git_text(sandbox, &["rev-parse", "HEAD^{commit}"])? != base
            || oracle_git_text(sandbox, &["rev-parse", "refs/heads/main^{commit}"])? != base
        {
            bail!("oracle clone 未绑定 captured main");
        }
        Ok(())
    };
    match setup() {
        Ok(()) => Ok(()),
        Err(error) => match remove_oracle_sandbox(root, sandbox) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(error.context(format!(
                "oracle clone setup cleanup 同时失败: {cleanup_error:#}"
            ))),
        },
    }
}

fn install_oracle_fixture_signoff(sandbox: &Path, round: &str) -> Result<()> {
    let ir = crate::plan::load_round_ir(sandbox, round)?;
    let digest = crate::plan::validation_digest(&ir);
    let events_rel = format!("coordination/rounds/{round}/events.jsonl");
    let events_path = sandbox.join(&events_rel);
    let read = read_ledger(&events_path)?;
    if !read.bad_lines.is_empty() {
        bail!("oracle clone ledger 含坏行，拒绝 fixture sign-off");
    }
    let validated = crate::plan::require_validated_round_ir(sandbox, round, &read.events)?;
    if validated.persisted_revision != ir.revision || validated.persisted_digest != digest {
        bail!("oracle clone fixture sign-off 未绑定 exact validation high-water");
    }
    if crate::plan::matching_user_plan_signoff_position(&read.events, round, ir.revision, &digest)?
        .is_none()
    {
        let signoff = ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some(round),
            crate::plan::plan_signed_off_payload(
                "oracle isolated-clone fixture-only predecessor",
                ir.revision,
                &digest,
            )
            .map_err(anyhow::Error::msg)?,
        );
        let mut bytes = fs::read(&events_path)
            .with_context(|| format!("读取 oracle clone ledger 失败: {}", events_path.display()))?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        serde_json::to_writer(&mut bytes, &signoff)
            .context("序列化 oracle fixture sign-off 失败")?;
        bytes.push(b'\n');
        fs::write(&events_path, bytes)
            .with_context(|| format!("写 oracle clone ledger 失败: {}", events_path.display()))?;
        oracle_git(sandbox, &["add", "--", &events_rel])?;
        let base = oracle_git_text(sandbox, &["rev-parse", "HEAD^{commit}"])?;
        let tree = oracle_git_text(sandbox, &["write-tree"])?;
        let commit = Command::new("git")
            .arg("-C")
            .arg(sandbox)
            .args(["commit-tree", &tree, "-p", &base])
            .args(["-m", "test(oracle): bind isolated preverify baseline"])
            .env("GIT_AUTHOR_NAME", "orch oracle fixture")
            .env("GIT_AUTHOR_EMAIL", "oracle-fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "orch oracle fixture")
            .env("GIT_COMMITTER_EMAIL", "oracle-fixture@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .context("git commit-tree oracle fixture sign-off 失败")?;
        if !commit.status.success() {
            bail!(
                "git commit-tree oracle fixture sign-off 失败({}): {}",
                commit.status,
                String::from_utf8_lossy(&commit.stderr)
            );
        }
        let fixture_commit = String::from_utf8(commit.stdout)
            .context("oracle fixture commit-tree stdout 非 UTF-8")?
            .trim()
            .to_string();
        if !is_full_commit_oid(&fixture_commit) {
            bail!("oracle fixture commit-tree 未返回完整 OID");
        }
        oracle_git(
            sandbox,
            &["update-ref", "refs/heads/main", &fixture_commit, &base],
        )?;
        if oracle_git_text(sandbox, &["rev-parse", &format!("{fixture_commit}^")])? != base {
            bail!("oracle fixture sign-off commit 不是 captured base 的单亲子提交");
        }
        let changed = oracle_git_text(sandbox, &["diff", "--name-only", &base, &fixture_commit])?;
        if changed != events_rel {
            bail!("oracle fixture sign-off commit 越界: {changed:?}");
        }
        eprintln!(
            "orch oracle: fixture-only clone sign-off · base={} · fixture={} · revision={} · digest={}",
            base, fixture_commit, ir.revision, digest
        );
    }
    let committed = read_ledger(&events_path)?;
    if !committed.bad_lines.is_empty()
        || crate::plan::matching_user_plan_signoff_position(
            &committed.events,
            round,
            ir.revision,
            &digest,
        )?
        .is_none()
        || oracle_git_text(sandbox, &["rev-parse", "HEAD^{commit}"])?
            != oracle_git_text(sandbox, &["rev-parse", "refs/heads/main^{commit}"])?
        || !oracle_git(
            sandbox,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?
        .is_empty()
    {
        bail!("oracle clone fixture sign-off 未形成 exact committed main identity");
    }
    Ok(())
}

pub(crate) fn preverify_observation(
    root: &Path,
    c: &card::Card,
    allow_file_passed: usize,
) -> Result<OracleObservation> {
    // 必须先于 binding/git/worktree 等任何外部动作；恶意 seed path 不得制造半成品现场。
    validate_seed_paths(root, c)?;
    let round = crate::current_round(root)?;
    if c.meta
        .round
        .as_deref()
        .is_some_and(|declared| declared != round)
    {
        bail!("oracle preverify card round does not match CURRENT-ROUND");
    }
    let b = binding::load(root)?;
    let base = gitx::rev_parse(root, "main")?;
    let root_head = gitx::rev_parse(root, "HEAD^{commit}")?;
    let root_status = oracle_git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let sandbox_label = format!("oracle-{}", c.meta.task_id);
    let wt = root
        .join(".worktrees")
        .join(crate::util::unique_scratch_name(&sandbox_label));
    provision_oracle_clone(root, &wt, &base)?;

    // 主体闭包：无论成败最后都清理临时 isolated clone。
    let body = || -> Result<OracleObservation> {
        // Assertion-red replays execute the full baseline suite, whose landed-
        // seed guards require a signed main. Compile-red replays stop before
        // any test binary runs, so installing a synthetic sign-off there is
        // unnecessary and breaks nested plan -> seed-verified fixtures whose
        // freshly generated IR intentionally has not been committed yet.
        if c.meta.red_form.as_deref() != Some("compile") {
            install_oracle_fixture_signoff(&wt, &round)?;
        }
        if gitx::rev_parse(root, "refs/heads/main^{commit}")? != base
            || gitx::rev_parse(root, "HEAD^{commit}")? != root_head
            || oracle_git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?
                != root_status
        {
            bail!("oracle clone fixture setup 改动了真实 root");
        }
        let nm = root.join("node_modules");
        if (b.oracle.dialect == "vitest" || b.has_ecosystem("node"))
            && nm.exists()
            && !wt.join("node_modules").exists()
        {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&nm, wt.join("node_modules")).ok();
        }
        copy_seeds_contained(root, &wt, c)?;
        let gate_ref = red_replay_gate(&c.meta.gates.fast).map_err(|e| anyhow::anyhow!("{e}"))?;
        run_test_gate_and_parse_observation_guarded(
            root,
            &round,
            &c.meta.task_id,
            &b,
            &wt,
            &c.meta.seeds,
            &gate_ref,
        )
    };
    let result = body();
    let cleanup = remove_oracle_sandbox(root, &wt);
    let root_refs_unchanged = gitx::rev_parse(root, "refs/heads/main^{commit}")? == base
        && gitx::rev_parse(root, "HEAD^{commit}")? == root_head;
    let observation = match (result, cleanup, root_refs_unchanged) {
        (Ok(observation), Ok(()), true) => observation,
        (Ok(_), Ok(()), false) => bail!("oracle clone gate/cleanup 改动了真实 root refs"),
        (Ok(_), Err(error), true) => return Err(error.context("oracle clone cleanup 失败")),
        (Ok(_), Err(error), false) => {
            return Err(error.context("oracle clone cleanup 失败且真实 root refs 漂移"))
        }
        (Err(error), Ok(()), true) => return Err(error),
        (Err(error), Ok(()), false) => {
            return Err(error.context("oracle clone gate 失败且真实 root refs 漂移"))
        }
        (Err(error), Err(cleanup_error), true) => {
            return Err(error.context(format!("oracle clone cleanup 同时失败: {cleanup_error:#}")))
        }
        (Err(error), Err(cleanup_error), false) => {
            return Err(error.context(format!(
                "oracle clone cleanup 同时失败且真实 root refs 漂移: {cleanup_error:#}"
            )))
        }
    };
    let m = &observation.measured;

    // O5 机器化判据
    if m.file_failed == 0 {
        bail!("oracle 预验失败：种子文件内 0 红——种子无检出力（或产品已在库，对照 O5 r4 教训）");
    }
    if m.file_passed > allow_file_passed {
        bail!(
            "oracle 预验失败：种子文件内 {} passed > 允许 {}（空洞通过位，O5）——逐用例核对：{:?}",
            m.file_passed,
            allow_file_passed,
            m.failed_cases
        );
    }
    // 编译红时 workspace 尚未运行；构造值令“总红 == 文件红”恒成立，沿用同一 O5 判据。
    if m.total_failed != m.file_failed {
        bail!(
            "oracle 预验失败：总红 {} ≠ 种子文件红 {}——种子殃及既有用例（基线受扰）",
            m.total_failed,
            m.file_failed
        );
    }
    Ok(observation)
}

/// Legacy measured-only API. This deliberately does not rerun the gate:
/// callers receive a projection of the complete observation.
pub fn preverify(root: &Path, c: &card::Card, allow_file_passed: usize) -> Result<Measured> {
    preverify_observation(root, c, allow_file_passed).map(|observation| observation.measured)
}

/// 从 REPORT 提取 §3（种子/先红）段落文本（`## 3` 起到下一个 `## ` 止）
fn extract_report_section3(report: &str) -> Option<String> {
    let mut in_s3 = false;
    let mut out = String::new();
    for line in report.lines() {
        if line.trim_start().starts_with("## ") {
            if in_s3 {
                break;
            }
            // 标题风格容忍（r16/B28 实证：执行者写 `## §3` 导致段落定位落空、误亮叙事失职黄旗）
            let t = line
                .trim_start()
                .trim_start_matches("## ")
                .trim_start_matches('§');
            in_s3 = t.starts_with('3');
            continue;
        }
        if in_s3 {
            out.push_str(line);
            out.push('\n');
        }
    }
    (!out.is_empty()).then_some(out)
}

fn parse_suite_counts_for_dialect(dialect: &str, log: &str) -> Option<SuiteCounts> {
    match dialect {
        "cargo" => parse_cargo_suite_counts(log),
        "vitest" => parse_suite_counts(log),
        _ => None,
    }
}

/// 机检级先红复跑（收取阶段调用）——E12 零模型兜底的两半：
/// ①事实核验：seed commit 处复跑测试门，与账本预验基线比对（防实现混入/种子漂移/坏基线）；
/// ②叙事核验：REPORT §3 声称的 vitest 计数（若可解析）与实测比对（防报数造假——r6/B9 实测：
///   只核事实拦不住「事实真+叙事谎」，必须读叙事数字）。
/// 不符 = bail（机检 FAIL）；旧轮无 measured 基线则记录并放行（向后兼容）。
/// `gate_run_id` 与 `scoped_gate_tag` 必须精确绑定本 round/task/attempt 的 red-replay phase；
/// 身份不一致时在 spawn 前拒绝，避免日志与 durable observation 指向不同的运行。
pub fn replay_seed_red(
    root: &Path,
    round: &str,
    c: &card::Card,
    branch: &str,
    worktree: &Path,
    attempt_id: &str,
    storage_permit: &crate::storage::StoragePermit,
    gate_run_id: &str,
    scoped_gate_tag: &str,
) -> Result<()> {
    if c.meta.seeds.is_empty() {
        return Ok(());
    }
    let expected_gate_tag = gate::phase_scoped_log_tag(
        round,
        &c.meta.task_id,
        attempt_id,
        gate::GatePhase::RedReplay,
        gate_run_id,
    );
    if scoped_gate_tag != expected_gate_tag {
        bail!("red replay gate tag 未绑定 typed round/task identity");
    }
    validate_seed_paths(root, c)?;
    let b = binding::load(root)?;
    let mb = gitx::merge_base(root, "main", branch)?;
    let seed_sha = gitx::commits_after(root, &mb, branch)?
        .into_iter()
        .next()
        .context("先红复跑：分支无提交（种子 commit 缺失）")?;

    // Task-scoped history is required for schema high-water enforcement; a
    // latest-only lookup cannot detect v2 -> versionless downgrade.
    let lr = read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))?;
    let task_payloads = lr
        .events
        .iter()
        .filter(|event| {
            event.kind == "SeedOracleVerified" && event.task_id.as_deref() == Some(&c.meta.task_id)
        })
        .filter_map(|event| event.payload.clone())
        .collect::<Vec<_>>();
    let baseline = task_payloads
        .last()
        .and_then(|p| p.get("measured").cloned());

    // detached 到 seed commit → 跑测试门 → 无论成败先回分支头再传播错误（现场必须复原）
    let gate_ref = red_replay_gate(&c.meta.gates.fast).map_err(|e| anyhow::anyhow!("{e}"))?;
    gitx::checkout(worktree, &seed_sha)?;
    let replay = run_test_gate_and_parse_observation_with_permit(
        root,
        round,
        crate::ledger::GateAuditIdentity::Attempt {
            task_id: &c.meta.task_id,
            attempt_id,
        },
        storage_permit,
        &b,
        worktree,
        gate_run_id,
        scoped_gate_tag,
        &c.meta.seeds,
        &gate_ref,
    );
    gitx::checkout(worktree, branch)?;
    let observation = replay?;
    let m = &observation.measured;

    // ② 叙事核验：REPORT §3 的声称计数（可解析才比对；无数字行留给 verifier 兜）
    let report_rel = format!(
        "coordination/rounds/{round}/reports/{}-REPORT.md",
        c.meta.task_id
    );
    let report_text = fs::read_to_string(worktree.join(&report_rel))
        .or_else(|_| fs::read_to_string(root.join(&report_rel)))
        .unwrap_or_default();
    if m.red_form.as_deref() == Some("compile") {
        // 编译红形态：§3 无计数可言（记录的是 error 关键行）——数字比对不适用（E13：
        // r8/B12 实测「无任何 `test result:` 行」的叙述句被当计数行解析出 0f/0p 误杀诚实 REPORT）。
        // 叙事核验降为 error 关键行存在性；内容真伪由 verifier 深核。
        let s3 = extract_report_section3(&report_text).unwrap_or_default();
        if s3.contains("error[") || s3.contains("error:") {
            println!("④½ 叙事核验通过: 编译红形态——REPORT §3 含 error 关键行（数字比对不适用）");
        } else {
            println!("⚠️ 编译红棒 REPORT §3 未见 error 关键行——叙事失职嫌疑，留给 verifier 深核");
        }
    } else if let Some(claim) = extract_report_section3(&report_text)
        .and_then(|section| parse_suite_counts_for_dialect(&b.oracle.dialect, &section))
    {
        if let Err(reason) = report_claim_ok(&claim, &m) {
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({"stage": "red-replay-claim", "reason": &reason}),
                )],
            )?;
            bail!(
                "机检 FAIL·REPORT §3 报数造假（E12 零模型拦截）：seed commit {}：{}",
                gitx::short(&seed_sha),
                reason
            );
        }
        println!(
            "④½ 叙事核验通过: REPORT §3 声称 {}f/{}p == seed commit 文件内实测 {}f/{}p",
            claim.failed, claim.passed, m.file_failed, m.file_passed
        );
    }

    let oracle_match_result = if m.red_form.as_deref() == Some("compile") {
        let identity = observation
            .compile_identity
            .as_ref()
            .context("compile replay lacks canonical compileIdentity")?;
        let effective_rebaseline = latest_compile_rebaseline(&lr.events, round, &c.meta.task_id);
        effective_rebaseline.and_then(|effective_rebaseline| {
            let upstream = recorded_upstream_public_symbols(
                root,
                round,
                c,
                &lr.events,
                lr.bad_lines.is_empty(),
            );
            let upstream_symbols = upstream.symbols();
            let (_, seed_baseline) = compile_oracle_baseline_state(&task_payloads)?;
            let attribution_baseline = effective_rebaseline
                .as_ref()
                .or(seed_baseline.as_ref())
                .cloned();
            compile_oracle_replay_match(
                &task_payloads,
                effective_rebaseline.as_ref(),
                identity,
                &upstream_symbols,
            )
            .map(|mut decision| {
                if let (Some(evidence), Some(baseline)) = (
                    decision.legitimate_shrink.as_mut(),
                    attribution_baseline.as_ref(),
                ) {
                    evidence.upstream_tasks =
                        upstream.tasks_explaining(&evidence.dropped, baseline);
                }
                decision
            })
        })
    } else {
        assertion_oracle_baseline_match(&task_payloads).map(|matched| CompileReplayDecision {
            matched,
            legitimate_shrink: None,
        })
    };
    let decision = match oracle_match_result {
        Ok(decision) => decision,
        Err(reason) => {
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({"stage": "red-replay", "reason": &reason}),
                )],
            )?;
            bail!(
                "机检 FAIL·compile oracle identity/high-water 不符：seed commit {}：{}",
                gitx::short(&seed_sha),
                reason
            );
        }
    };
    let oracle_match = decision.matched;
    let legitimate_shrink = decision.legitimate_shrink;

    if let Some(base) = &baseline {
        let want = |key: &str| {
            base.get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX) as usize
        };
        let want_red_form = base.get("redForm").and_then(serde_json::Value::as_str);
        let red_form_matches = m.red_form.as_deref() == want_red_form
            || (b.oracle.dialect == "vitest"
                && m.red_form.is_none()
                && want_red_form == Some("assertion"));
        if m.file_failed != want("fileFailed")
            || m.file_passed != want("filePassed")
            || m.total_failed != want("totalFailed")
            || !red_form_matches
        {
            let reason = format!(
                "seed commit 实测 {}f/{}p(文件内) 总红{} redForm={:?} ≠ 预验基线 {}",
                m.file_failed, m.file_passed, m.total_failed, m.red_form, base
            );
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "EscalationRaised",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({"stage": "red-replay", "reason": &reason}),
                )],
            )?;
            bail!(
                "机检 FAIL·先红复跑不符（E12 零模型拦截）：seed commit {}：{}",
                gitx::short(&seed_sha),
                reason
            );
        }
    }

    let matches_oracle = match oracle_match {
        CompileOracleMatch::IdentityV2 => serde_json::json!(true),
        CompileOracleMatch::LegacyCountsOnly => {
            println!(
                "⚠️ ④½ LEGACY COUNTS-ONLY：任务 {} 尚无 compile identity 证明，仅按历史计数兼容回放",
                c.meta.task_id
            );
            serde_json::json!("legacy-counts-only")
        }
        CompileOracleMatch::NoBaseline => serde_json::json!("no-baseline"),
    };
    let red_proven = ledger::event(
        "RedProven",
        "runtime:orch",
        Some(&c.meta.task_id),
        Some(round),
        serde_json::json!({
        "failedCount": m.file_failed,
        "matchesOracle": matches_oracle,
        "seedCommit": gitx::short(&seed_sha),
        "replay": serde_json::to_value(&observation)?,
        }),
    );
    let events = if let Some(evidence) = &legitimate_shrink {
        let rebaseline = observation
            .compile_identity
            .as_ref()
            .context("legitimate compile shrink lacks rebaseline identity")?;
        vec![
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some(&c.meta.task_id),
                Some(round),
                serde_json::json!({
                    "stage": "red-replay",
                    "reason": LEGITIMATE_SHRINK_REASON,
                    "identityShift": "legitimate-shrink",
                    "baselineSymbols": evidence.baseline_symbols.clone(),
                    "replaySymbols": evidence.replay_symbols.clone(),
                    "dropped": evidence.dropped.clone(),
                    "upstreamTasks": evidence.upstream_tasks.clone(),
                    "rebaselineCompileIdentity": rebaseline,
                }),
            ),
            red_proven,
        ]
    } else {
        vec![red_proven]
    };
    ledger::append(root, round, &events)?;
    if let Some(evidence) = legitimate_shrink {
        println!(
            "④½ compile oracle 合法收缩并已重记基线: dropped={:?} upstreamTasks={:?}",
            evidence.dropped, evidence.upstream_tasks
        );
    }
    println!(
        "④½ 先红复跑通过: seed commit {} 实测 {}f/{}p(文件内)·总 {}f/{}p，oracle={:?}",
        gitx::short(&seed_sha),
        m.file_failed,
        m.file_passed,
        m.total_failed,
        m.total_passed,
        oracle_match
    );
    Ok(())
}

/// O5 种子局部性纯判据（r31/B60）：两条判据全过⇒Ok，否则 Err 指明哪条失败。
/// · 无空洞位：seed 文件内 passed = seed_file.tests - seed_file.failed，须 <= allow_file_passed；
/// · 基线不受扰：suite.failed == seed_file.failed（全部红都局部在种子文件，无附带红）。
/// additive seam——`preverify` 与既有测试不动。
pub fn seed_red_localized(
    suite: &SuiteCounts,
    seed_file: &FileCounts,
    allow_file_passed: usize,
) -> Result<(), String> {
    let file_passed = seed_file.tests - seed_file.failed;
    if file_passed > allow_file_passed {
        return Err(format!(
            "空洞位：种子文件内 {file_passed} passed > 允许 {allow_file_passed}（O5 无空洞位强制）"
        ));
    }
    if suite.failed != seed_file.failed {
        return Err(format!(
            "基线受扰：整套 {} failed ≠ 种子文件 {} failed（有附带红）",
            suite.failed, seed_file.failed
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_seed_runs_ignore_unrelated_harness_chatter() {
        let other = "Running tests/other.rs (target/debug/deps/other-0123456789abcdef)\nrunning 1 test\nrunning 99 tests\ntest result: FAILED. invalid counts\n";
        let selected = "Running tests/seed.rs (target/debug/deps/seed-0123456789abcdef)\nrunning 1 test\ntest one ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored\n";
        let seed = CargoSeedRunSpec { target: "orch/crates/fixture/tests/seed.rs".into(), test_names: vec!["one".into()] };
        let counts = observed_cargo_seed_counts(&format!("{other}{selected}{other}"), &[seed]).unwrap();
        assert_eq!((counts.passed, counts.failed), (1, 0));
    }

    #[test]
    fn observed_seed_run_cannot_be_closed_by_another_target() {
        let log = "Running tests/seed.rs (target/debug/deps/seed-0123456789abcdef)\nrunning 1 test\ntest one ... ok\nRunning tests/other.rs (target/debug/deps/other-0123456789abcdef)\nrunning 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored\n";
        let seed = CargoSeedRunSpec { target: "orch/crates/fixture/tests/seed.rs".into(), test_names: vec!["one".into()] };
        assert!(observed_cargo_seed_counts(log, &[seed]).is_err());
    }

    #[test]
    fn observed_summaries_reject_contradictory_or_duplicate_totals() {
        for summary in [
            "test result: FAILED. 1 passed; 0 failed; 0 ignored",
            "test result: ok. 0 passed; 1 failed; 0 ignored",
            "test result: ok. 1 passed; NaN passed; 0 failed; 0 ignored",
            "test result: ok. 1 passed; 1 passed; 0 failed; 0 ignored",
            "test result: ok.extra 1 passed; 0 failed; 0 ignored",
        ] {
            assert!(cargo_observed_summary(summary).is_none(), "{summary}");
        }
        assert_eq!(cargo_observed_summary("test result: ok. 1 passed; 0 failed; 0 ignored"), Some((1, 0, 0)));
        assert_eq!(cargo_observed_summary("test result: FAILED. 0 passed; 1 failed; 0 ignored"), Some((0, 1, 0)));
    }

    #[test]
    fn observed_seed_target_components_must_be_canonical() {
        let log = "Running tests/seed.rs (target/debug/deps/seed-0123456789abcdef)\nrunning 1 test\ntest one ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored\n";
        for target in ["orch/crates//tests/seed.rs", "orch/crates/../tests/seed.rs", "orch/crates/a b/tests/seed.rs"] {
            let seed = CargoSeedRunSpec { target: target.into(), test_names: vec!["one".into()] };
            assert!(observed_cargo_seed_counts(log, &[seed]).is_err());
        }
    }

    #[test]
    fn observed_seed_counts_do_not_reuse_one_run_for_two_packages() {
        let log = "Running tests/same.rs (target/debug/deps/same-0123456789abcdef)\nrunning 1 test\ntest one ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored\n";
        let seeds = ["first", "second"].map(|package| CargoSeedRunSpec {
            target: format!("orch/crates/{package}/tests/same.rs"),
            test_names: vec!["one".into()],
        });
        assert!(observed_cargo_seed_counts(log, &seeds).is_err());
    }

    #[test]
    fn workspace_source_ownership_rejects_a_foreign_same_named_target() {
        let root = std::env::current_dir().unwrap().join(".cowork-temp")
            .join(format!("B339-target-owners-{}", ulid::Ulid::new()));
        let target = "orch/crates/first/tests/same.rs".to_string();
        fs::create_dir_all(root.join("orch/crates/first/tests")).unwrap();
        fs::write(root.join(&target), "#[test]\nfn one() {}\n").unwrap();
        assert!(validate_cargo_seed_target_owners(&root, std::slice::from_ref(&target)).is_ok());
        fs::create_dir_all(root.join("orch/crates/second/tests")).unwrap();
        fs::write(root.join("orch/crates/second/tests/same.rs"), "#[test]\nfn one() {}\n").unwrap();
        let error = validate_cargo_seed_target_owners(&root, &[target]).unwrap_err();
        assert!(error.to_string().contains("seed-not-attributable"));
        fs::remove_dir_all(root).unwrap();
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ExpectedDynamicAnchor {
        original_event_id: String,
        original_sha256: String,
        digest: Option<String>,
        effective: card::FrozenEffectiveAnchor,
    }

    fn expected_recorded_dynamic_anchors(
        root: &Path,
        main_oid: &str,
        events: &[EventRecord],
        genesis_relocation_ids: &BTreeSet<String>,
        genesis_anchors: &BTreeMap<String, (Option<String>, card::FrozenEffectiveAnchor)>,
    ) -> Result<BTreeMap<String, ExpectedDynamicAnchor>> {
        let mut known = genesis_anchors.clone();
        let mut dynamic = BTreeMap::<String, ExpectedDynamicAnchor>::new();

        // Keep this delta predicate in lockstep with load_signed_baseline's
        // immutable-genesis replay block above.
        for (position, event) in events.iter().enumerate() {
            if event.kind == "SeedRelocated" {
                if genesis_relocation_ids.contains(&event.event_id) {
                    continue;
                }
                let payload: SeedRelocatedPayload = serde_json::from_value(
                    event
                        .payload
                        .clone()
                        .context("dynamic SeedRelocated 缺 payload")?,
                )
                .context("dynamic SeedRelocated payload 非 closed schema")?;
                let (Some(round), Some(task_id)) =
                    (event.round.as_deref(), event.task_id.as_deref())
                else {
                    bail!("dynamic SeedRelocated 缺 round/taskId: {}", event.event_id);
                };
                validate_relocation_event(
                    events,
                    &event.event_id,
                    round,
                    task_id,
                    &payload.target,
                    &payload.sha256,
                )?;
                canonical_repo_relative(&payload.target, "dynamic landed target")?;
                let recorded = events[position + 1..].iter().any(|candidate| {
                    candidate.kind == "TaskRecorded"
                        && candidate.actor == "runtime:orch"
                        && candidate.round.as_deref() == Some(round)
                        && candidate.task_id.as_deref() == Some(task_id)
                        && candidate
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("postMergeGates"))
                            .and_then(serde_json::Value::as_str)
                            == Some("all-green")
                });
                if !recorded {
                    continue;
                }

                match known.entry(payload.target.clone()) {
                    std::collections::btree_map::Entry::Occupied(mut existing) => {
                        if existing.get().0.as_deref() != Some(&payload.sha256) {
                            bail!(
                                "dynamic SeedRelocated 不得覆盖既有 frozen anchor（须 FrozenContractSuperseded）: {}",
                                payload.target
                            );
                        }
                        if existing.get().1.kind == "seed-relocated" {
                            existing.get_mut().1.event_id = Some(event.event_id.clone());
                            if let Some(expected) = dynamic.get_mut(&payload.target) {
                                expected.effective.event_id = Some(event.event_id.clone());
                            }
                        }
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let effective = card::FrozenEffectiveAnchor {
                            kind: "seed-relocated".to_string(),
                            event_id: Some(event.event_id.clone()),
                            baseline_tree_sha: None,
                            sha256: Some(payload.sha256.clone()),
                        };
                        entry.insert((Some(payload.sha256.clone()), effective.clone()));
                        dynamic.insert(
                            payload.target,
                            ExpectedDynamicAnchor {
                                original_event_id: event.event_id.clone(),
                                original_sha256: payload.sha256.clone(),
                                digest: Some(payload.sha256),
                                effective,
                            },
                        );
                    }
                }
            } else if event.kind == "FrozenContractSuperseded" {
                let payload =
                    crate::verify::validate_frozen_contract_supersession_delta_for_replay(
                        root, main_oid, events, position,
                    )?;
                if !is_canonical_sha256(&payload.new_file_sha256) {
                    bail!("supersession newFileSha256 非 canonical");
                }
                let known_anchor = known.get_mut(&payload.target).with_context(|| {
                    format!("supersession target 无 genesis: {}", payload.target)
                })?;
                if known_anchor.0.as_deref() != Some(&payload.old_file_sha256)
                    || known_anchor.1 != payload.effective_anchor
                {
                    bail!(
                        "supersession effective-anchor chain mismatch: {}",
                        payload.target
                    );
                }
                let effective = card::FrozenEffectiveAnchor {
                    kind: "frozen-contract-superseded".to_string(),
                    event_id: Some(event.event_id.clone()),
                    baseline_tree_sha: None,
                    sha256: Some(payload.new_file_sha256.clone()),
                };
                known_anchor.0 = Some(payload.new_file_sha256.clone());
                known_anchor.1 = effective.clone();
                if let Some(expected) = dynamic.get_mut(&payload.target) {
                    expected.digest = Some(payload.new_file_sha256);
                    expected.effective = effective;
                }
            }
        }
        Ok(dynamic)
    }

    const LOG_RED: &str = "\
 ❯ test/product.test.ts (5 tests | 5 failed) 3ms
   × product (contract B6) > ① multiplies values 2ms
   × product (contract B6) > ② empty array → 1 (multiplicative identity) 0ms
   × product (contract B6) > ③ zero annihilates 0ms
   × product (contract B6) > ④ handles negatives 0ms
   × product (contract B6) > ⑤ does not mutate the input array 0ms
 Test Files  1 failed | 6 passed (7)
      Tests  5 failed | 33 passed (38)
";
    const LOG_GREEN: &str = "\
 ✓ test/stddev.test.ts (5 tests) 4ms
 Test Files  8 passed (8)
      Tests  43 passed (43)
";

    fn quoted_hash(hash: &str) -> String {
        format!("before \"{hash}\" after")
    }

    #[test]
    fn one_hash_swap_allows_shared_nibbles_and_unchanged_hashes() {
        let old = format!("a{}f", "1".repeat(62));
        let new = format!("a{}f", "2".repeat(62));
        let untouched = "3".repeat(64);
        let before = format!("{} / {}", quoted_hash(&untouched), quoted_hash(&old));
        let after = format!("{} / {}", quoted_hash(&untouched), quoted_hash(&new));
        assert_eq!(single_hash_literal_swap(&before, &after), Some((old, new)));
    }

    #[test]
    fn hash_swap_is_byte_exact_and_canonical() {
        let old = "a".repeat(64);
        let new = "2".repeat(64);
        assert!(single_hash_literal_swap(&quoted_hash(&old), &quoted_hash(&old)).is_none());
        assert!(
            single_hash_literal_swap(&quoted_hash(&old), &format!("{}!", quoted_hash(&new)))
                .is_none()
        );
        assert!(single_hash_literal_swap(
            &quoted_hash(&format!("{}0", old)),
            &quoted_hash(&format!("{}0", new))
        )
        .is_none());
        assert!(
            single_hash_literal_swap(&quoted_hash(&old.to_uppercase()), &quoted_hash(&new))
                .is_none()
        );
        assert_eq!(
            single_hash_literal_swap(
                &format!("中文{}尾", quoted_hash(&old)),
                &format!("中文{}尾", quoted_hash(&new))
            ),
            Some((old, new))
        );
        let old = "a".repeat(64);
        let new = "2".repeat(64);
        assert!(single_hash_literal_swap(
            &format!("left-A {} right", quoted_hash(&old)),
            &format!("left-B {} right", quoted_hash(&new)),
        )
        .is_none());
    }

    #[test]
    fn supersession_authorization_rejects_missing_or_malformed_anchors() {
        let digest = "a".repeat(64);
        assert!(frozen_contract_supersession_authorized(
            "runtime:orch",
            true,
            None,
            &digest,
            &digest
        )
        .is_err());
        assert!(frozen_contract_supersession_authorized(
            "runtime:orch",
            true,
            Some(&digest.to_uppercase()),
            &digest,
            &digest
        )
        .is_err());
        assert!(
            frozen_contract_supersession_authorized("runtime:orch", true, Some(""), "", "")
                .is_err()
        );
        let other = "b".repeat(64);
        assert!(frozen_contract_supersession_authorized(
            "runtime:orch",
            true,
            Some(&digest),
            &other,
            &digest,
        )
        .is_err());
    }

    #[test]
    fn signed_landed_seed_manifest_replays_at_its_historical_policy_base() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest_dir.ancestors().nth(3).unwrap();
        if !gitx::branch_exists(root, "main") {
            return;
        }
        let history = orch_core::read_ledger(&root.join("coordination/rounds/r82/events.jsonl")).unwrap();
        assert!(history.bad_lines.is_empty());
        let main = history.events.iter().find_map(|event| {
            let payload = event.payload.as_ref()?;
            (event.kind == "DispatchIssued" && event.task_id.as_deref() == Some("B312")
                && payload["attemptId"] == "B312-A0001")
                .then(|| payload["baseSha"].as_str().unwrap().to_owned())
        }).expect("immutable post-B311 legacy policy base");
        let binding_bytes =
            gitx::show_bytes(root, &main, "coordination/PROJECT-BINDING.yaml").unwrap();
        let bound = binding::parse_binding_bytes(&binding_bytes).unwrap();
        let Some(descriptor) = bound.oracle.landed_seed_baseline.as_ref() else {
            return;
        };
        let manifest_bytes = gitx::show_bytes(root, &main, &descriptor.path).unwrap();
        binding::parse_landed_seed_baseline(descriptor, &manifest_bytes).unwrap();
        assert_eq!(sha256_hex(&manifest_bytes), descriptor.sha256);
        let manifest: BaselineManifest = serde_json::from_slice(&manifest_bytes).unwrap();
        let genesis_anchors = manifest
            .targets
            .iter()
            .map(|target| {
                (
                    target.target.clone(),
                    (
                        target.effective_sha256.clone(),
                        baseline_anchor_as_card(&target.effective_anchor),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(manifest.targets.len(), 225);
        assert_eq!(genesis_anchors.len(), manifest.targets.len());
        assert_eq!(
            manifest.counts.unique_effective_targets_through_b269,
            manifest.targets.len()
        );
        assert_eq!(
            manifest
                .targets
                .iter()
                .filter(|target| target.effective_sha256.is_none())
                .count(),
            1
        );
        assert_eq!(
            manifest
                .targets
                .iter()
                .filter(|target| target.effective_anchor.kind == "migration-tombstone")
                .count(),
            1
        );
        assert_eq!(
            manifest
                .targets
                .iter()
                .filter(|target| target.effective_anchor.kind == "migration-baseline")
                .count(),
            34
        );

        let genesis_relocation_ids = manifest
            .targets
            .iter()
            .flat_map(|target| &target.sources)
            .flat_map(|source| &source.seed_relocated)
            .map(|relocation| relocation.event_id.clone())
            .collect::<BTreeSet<_>>();
        let events = committed_tree_events(root, &main).unwrap();
        let (anchors, _) = load_signed_baseline(root, &main).unwrap();
        if anchors.is_empty() {
            return;
        }
        let dynamic = expected_recorded_dynamic_anchors(
            root,
            &main,
            &events,
            &genesis_relocation_ids,
            &genesis_anchors,
        )
        .unwrap();
        assert!(
            !dynamic.is_empty(),
            "current tree must exercise at least one recorded post-genesis relocation"
        );
        let expected_targets = genesis_anchors
            .keys()
            .chain(dynamic.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let actual_targets = anchors.keys().cloned().collect::<BTreeSet<_>>();
        assert_eq!(
            actual_targets, expected_targets,
            "effective targets must exactly equal signed genesis plus recorded dynamic relocations"
        );
        let pending =
            crate::verify::pending_frozen_record_context_from_committed_main(root, &main, &events)
                .unwrap()
                .map(|(_, card)| {
                    card.meta
                        .frozen_contract_supersessions
                        .into_iter()
                        .map(|declaration| (declaration.target, declaration.new_file_sha256))
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
        for (target, expected) in &dynamic {
            let actual = anchors.get(target).unwrap();
            assert_eq!(
                actual.original_event_id, expected.original_event_id,
                "{target}"
            );
            assert_eq!(actual.original_sha256, expected.original_sha256, "{target}");
            assert!(!actual.grandfathered_drift, "{target}");
            if let Some(pending_digest) = pending.get(target) {
                assert_eq!(actual.digest.as_deref(), Some(pending_digest.as_str()));
            } else {
                assert_eq!(actual.digest, expected.digest, "{target}");
            }
            assert_eq!(actual.effective, expected.effective, "{target}");
        }
        for (target, expected) in &anchors {
            let digest = if gitx::tree_path_exists(root, &main, target).unwrap() {
                Some(sha256_hex(&gitx::show_bytes(root, &main, target).unwrap()))
            } else {
                None
            };
            assert_eq!(
                digest,
                expected.digest,
                "migration baseline drifted: {target}"
            );
        }
    }

    #[test]
    fn parses_red_suite_and_file() {
        let s = parse_suite_counts(LOG_RED).unwrap();
        assert_eq!((s.failed, s.passed, s.total), (5, 33, 38));
        let f = parse_file_counts(LOG_RED, "test/product.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 5));
        assert_eq!(parse_failed_cases(LOG_RED).len(), 5);
    }

    #[test]
    fn parses_green_suite_and_file() {
        let s = parse_suite_counts(LOG_GREEN).unwrap();
        assert_eq!((s.failed, s.passed, s.total), (0, 43, 43));
        let f = parse_file_counts(LOG_GREEN, "test/stddev.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 0));
        assert!(parse_failed_cases(LOG_GREEN).is_empty());
    }

    #[test]
    fn hollow_pass_detected_by_file_counts() {
        // O5 空洞：文件 5 用例只 4 红 → file_passed=1（r1/B3 的 4f/1p 场景可机判）
        let log = " ❯ test/span.test.ts (5 tests | 4 failed) 3ms\n      Tests  4 failed | 29 passed (33)\n";
        let f = parse_file_counts(log, "test/span.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 4));
    }

    #[test]
    fn single_case_file_singular_form() {
        // vitest 单用例文件是「(1 test)」单数（r6 负向自证实测踩到）
        let log = " ✓ test/hollow.test.ts (1 test) 1ms\n      Tests  44 passed (44)\n";
        let f = parse_file_counts(log, "test/hollow.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (1, 0));
    }

    #[test]
    fn report_section3_claim_extraction() {
        let report = "\
---
taskId: B9
---
## 1 变更文件清单
略
## 3 种子搬运证据（先红原文）
cmp 一致。seed commit 后：Tests  4 failed | 44 passed (48)
## 4 快门实测
npx vitest run: Tests  48 passed (48)
";
        let claim = extract_report_section3(report)
            .and_then(|s| parse_suite_counts(&s))
            .unwrap();
        // 只取 §3 段内的计数（§4 的 48 passed 不得混入）
        assert_eq!((claim.failed, claim.passed, claim.total), (4, 44, 48));
    }

    #[test]
    fn failure_detail_location_lines_do_not_hijack() {
        // 失败详情区的位置行（❯ target:15:5）不得被当成文件计数行（r6/B9 实测踩坑）
        let log = "\
 ❯ test/sumsq.test.ts (5 tests | 5 failed) 3ms
 Test Files  1 failed | 8 passed (9)
      Tests  5 failed | 43 passed (48)
 FAIL  test/sumsq.test.ts > sumsq (contract B9) > ① sums squares
 ❯ test/sumsq.test.ts:15:5
     15|   it(\"① sums squares\", () => {
";
        let f = parse_file_counts(log, "test/sumsq.test.ts").unwrap();
        assert_eq!((f.tests, f.failed), (5, 5));
    }

    #[test]
    fn cargo_suite_counts_sum_every_test_binary() {
        let log = "\
test result: ok. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n\
test result: FAILED. 7 passed; 2 failed; 1 ignored; 0 measured; 0 filtered out\n";
        let counts = parse_cargo_suite_counts(log).unwrap();
        assert_eq!((counts.failed, counts.passed, counts.total), (3, 10, 13));
    }

    #[test]
    fn cargo_failed_cases_are_deduplicated() {
        let log = "\
test oracle::tests::first ... FAILED\n\
test oracle::tests::second ... FAILED\n\
test oracle::tests::first ... FAILED\n";
        assert_eq!(
            parse_cargo_failed_cases(log),
            vec!["oracle::tests::first", "oracle::tests::second"]
        );
    }

    #[test]
    fn cargo_file_counts_extract_test_functions_across_attributes() {
        let source = "\
#[test]\n\
fn plain_case() {}\n\n\
#[test]\n\
#[should_panic(expected = \"boom\")]\n\
fn panic_case() {}\n";
        let counts = cargo_file_counts(source, &[]);
        assert_eq!((counts.tests, counts.failed), (2, 0));
    }

    #[test]
    fn cargo_file_counts_matches_qualified_failure_suffixes() {
        let source = "#[test]\nfn catches_bug() {}\n#[test]\nfn stays_green() {}\n";
        let failed = vec![
            "oracle::tests::catches_bug".to_string(),
            "unrelated::case".to_string(),
        ];
        let counts = cargo_file_counts(source, &failed);
        assert_eq!((counts.tests, counts.failed), (2, 1));
    }

    #[test]
    fn rust_compile_message_fallback_is_stable_and_not_code_only() {
        let first = canonical_rust_compile_identity(
            "error[E0308]: expected 12 values at /private/tmp/a/seed.rs:9:7\n",
        )
        .unwrap();
        let second = canonical_rust_compile_identity(
            "error[E0308]:  expected 900 values at /another/worktree/seed.rs:77:3\n",
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.diagnostics[0].keys,
            vec!["message:expected <n> values at <path>"]
        );
    }

    #[test]
    fn malformed_compile_identity_deserialization_fails_closed() {
        for value in [
            serde_json::json!({"dialect":"rustc","diagnostics":[]}),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"e0432","keys":["symbol:crate::missing"]}]
            }),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"E0432","keys":["symbol:z","symbol:a"]}]
            }),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"E0432","keys":["symbol:a","symbol:a"]}]
            }),
            serde_json::json!({
                "dialect":"rustc",
                "diagnostics":[{"code":"E0432","keys":["symbol:a"],"extra":true}]
            }),
        ] {
            assert!(
                serde_json::from_value::<CompileRedIdentity>(value).is_err(),
                "malformed identity was accepted"
            );
        }
    }

    #[test]
    fn expected_red_syntax_and_assertion_count_are_closed() {
        assert!(
            validate_expected_red_syntax(Some("compile"), "cargo", "error[E0432]", false).is_ok()
        );
        assert!(
            validate_expected_red_syntax(Some("compile"), "vitest", "error[E0432]", false).is_err()
        );
        assert!(validate_expected_red_syntax(Some("assertion"), "cargo", "2", false).is_ok());
        assert!(validate_expected_red_syntax(Some("assertion"), "cargo", "2 of 3", false).is_err());
        assert!(validate_expected_red_syntax(Some("unknown"), "cargo", "2", false).is_err());
        assert!(validate_expected_red_syntax(None, "cargo", "error[E0432]", false).is_err());
        assert!(
            validate_expected_red_syntax(Some("compile"), "cargo", "error[E0432]", true).is_err()
        );
    }

    #[test]
    fn unknown_schema_and_v2_downgrade_fail_closed() {
        let identity =
            canonical_rust_compile_identity("error[E0432]: unresolved import `crate::api`\n")
                .unwrap();
        let proof = validate_expected_compile_red("error[E0432]", &identity).unwrap();
        let modern = serde_json::json!({
            "oracleSchemaVersion": 2,
            "expectedRed": "error[E0432]",
            "expectedRedProof": proof,
            "measured": {
                "redForm": "compile",
                "compileIdentity": identity
            }
        });
        let legacy = serde_json::json!({
            "measured": {"fileFailed":1,"redForm":"compile"}
        });
        let unknown = serde_json::json!({"oracleSchemaVersion": 3});
        assert!(compile_oracle_baseline_match(
            &[unknown],
            &canonical_rust_compile_identity("error[E0432]: unresolved import `crate::api`\n")
                .unwrap()
        )
        .is_err());
        assert!(compile_oracle_baseline_match(
            &[modern, legacy],
            &canonical_rust_compile_identity("error[E0432]: unresolved import `crate::api`\n")
                .unwrap()
        )
        .is_err());
    }

    fn compile_identity(code: &str, keys: &[&str]) -> CompileRedIdentity {
        CompileRedIdentity {
            dialect: "rustc".to_string(),
            diagnostics: vec![CompileDiagnostic {
                code: code.to_string(),
                keys: keys.iter().map(|key| (*key).to_string()).collect(),
            }],
        }
    }

    fn v2_compile_payload(identity: &CompileRedIdentity) -> serde_json::Value {
        let proof = validate_expected_compile_red("error[E0432]", identity).unwrap();
        serde_json::json!({
            "oracleSchemaVersion": 2,
            "expectedRed": "error[E0432]",
            "expectedRedProof": proof,
            "measured": {
                "redForm": "compile",
                "compileIdentity": identity,
            }
        })
    }

    #[test]
    fn compile_replay_accepts_only_a_structurally_exact_explained_shrink() {
        let baseline = compile_identity("E0432", &["symbol:crate::kept", "symbol:crate::upstream"]);
        let replay = compile_identity("E0432", &["symbol:crate::kept"]);
        let payload = v2_compile_payload(&baseline);
        let upstream = vec!["symbol:crate::upstream".to_string()];
        let accepted = compile_oracle_replay_match(&[payload.clone()], None, &replay, &upstream)
            .expect("fully explained symbol-only shrink");
        assert_eq!(accepted.matched, CompileOracleMatch::IdentityV2);
        assert_eq!(
            accepted.legitimate_shrink.unwrap().dropped,
            upstream,
            "the durable evidence must enumerate every dropped symbol"
        );

        let unexplained = compile_oracle_replay_match(&[payload.clone()], None, &replay, &[])
            .expect_err("missing upstream evidence must fail closed");
        assert!(unexplained.starts_with("compile identity mismatch: baseline="));

        let changed_code = compile_identity("E0308", &["symbol:crate::kept"]);
        let structural_change =
            compile_oracle_replay_match(&[payload], None, &changed_code, &upstream)
                .expect_err("a code change cannot hide behind a symbol shrink");
        assert!(structural_change.starts_with("compile identity mismatch: baseline="));
    }

    #[test]
    fn legitimate_escalation_round_trips_as_a_rebaseline() {
        let replay = compile_identity("E0432", &["symbol:crate::kept"]);
        let payload = serde_json::json!({
            "stage": "red-replay",
            "reason": LEGITIMATE_SHRINK_REASON,
            "identityShift": "legitimate-shrink",
            "baselineSymbols": ["symbol:crate::kept", "symbol:crate::upstream"],
            "replaySymbols": ["symbol:crate::kept"],
            "dropped": ["symbol:crate::upstream"],
            "upstreamTasks": ["B197"],
            "rebaselineCompileIdentity": replay,
        });
        assert_eq!(
            legitimate_shrink_rebaseline(&payload).unwrap(),
            Some(replay.clone())
        );

        let mut malformed = payload;
        malformed["upstreamTasks"] = serde_json::json!([]);
        assert!(legitimate_shrink_rebaseline(&malformed).is_err());
    }

    #[test]
    fn public_symbol_evidence_excludes_impl_and_restricted_items() {
        let source = r#"pub struct Exported;
impl Exported {
    pub fn method_is_not_a_module_export() {}
}
pub(crate) fn restricted() {}
pub async fn run() {}
pub const VALUE: usize = 1;
"#;
        assert_eq!(
            public_symbols_in_source("orch/crates/orch-host/src/wake.rs", source),
            [
                "symbol:orch_host::wake::Exported".to_string(),
                "symbol:orch_host::wake::VALUE".to_string(),
                "symbol:orch_host::wake::run".to_string(),
                "symbol:wake::Exported".to_string(),
                "symbol:wake::VALUE".to_string(),
                "symbol:wake::run".to_string(),
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            public_module_prefix("orch/crates/orch-core/src/lib.rs").as_deref(),
            Some("orch_core")
        );
        assert!(public_module_prefix("orch/crates/orch-cli/src/main.rs").is_none());
    }

    #[test]
    fn r77_recorded_dependency_exposes_the_grouped_import_alias() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest_dir.ancestors().nth(3).unwrap();
        if !gitx::branch_exists(root, "main") {
            return;
        }
        let ledger = read_ledger(&root.join("coordination/rounds/r77/events.jsonl")).unwrap();
        let card = card::load(root, "r77", "B294").unwrap();
        let evidence = recorded_upstream_public_symbols(
            root,
            "r77",
            &card,
            &ledger.events,
            ledger.bad_lines.is_empty(),
        );
        assert_eq!(card.meta.depends_on, ["B293"]);
        assert!(
            evidence
                .by_task
                .get("B293")
                .is_some_and(|symbols| symbols.contains("symbol:harness::registry_digest")),
            "B293 recorded evidence must explain B294 grouped import: {:?}",
            evidence.by_task
        );
    }

    #[test]
    fn r77_b294_replay_is_a_legitimate_recorded_dependency_shrink() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest_dir.ancestors().nth(3).unwrap();
        if !gitx::branch_exists(root, "main") {
            return;
        }
        let ledger = read_ledger(&root.join("coordination/rounds/r77/events.jsonl")).unwrap();
        let card = card::load(root, "r77", "B294").unwrap();
        let task_payloads = ledger
            .events
            .iter()
            .filter(|event| {
                event.kind == "SeedOracleVerified" && event.task_id.as_deref() == Some("B294")
            })
            .filter_map(|event| event.payload.clone())
            .collect::<Vec<_>>();
        let evidence = recorded_upstream_public_symbols(
            root,
            "r77",
            &card,
            &ledger.events,
            ledger.bad_lines.is_empty(),
        );
        let replay = CompileRedIdentity {
            dialect: "rustc".to_string(),
            diagnostics: vec![
                CompileDiagnostic {
                    code: "E0425".to_string(),
                    keys: vec![
                        "symbol:ENVELOPE_KEYS".to_string(),
                        "symbol:harness".to_string(),
                    ],
                },
                CompileDiagnostic {
                    code: "E0425".to_string(),
                    keys: vec![
                        "symbol:NO_REVIEW_OUTPUT".to_string(),
                        "symbol:harness".to_string(),
                    ],
                },
                CompileDiagnostic {
                    code: "E0432".to_string(),
                    keys: vec!["symbol:orch_host::harness::InvocationEnvelope".to_string()],
                },
                CompileDiagnostic {
                    code: "E0583".to_string(),
                    keys: vec!["symbol:harness_invocation_envelope_support".to_string()],
                },
            ],
        };
        let upstream_symbols = evidence.symbols();
        let with_aliases = upstream_symbols_with_grouped_import_aliases(
            &v2_compile_identity(task_payloads.last().unwrap()).unwrap(),
            &upstream_symbols,
        );
        assert!(
            with_aliases.contains("symbol:registry_digest"),
            "grouped-import context must explain the bare item: {with_aliases:?}"
        );
        assert!(
            !upstream_symbols_with_grouped_import_aliases(
                &v2_compile_identity(task_payloads.last().unwrap()).unwrap(),
                &["symbol:other::registry_digest".to_string()],
            )
            .contains("symbol:registry_digest"),
            "an unrelated module with the same item name must not explain the shrink"
        );
        let decision =
            compile_oracle_replay_match(&task_payloads, None, &replay, &upstream_symbols).unwrap();
        let shrink = decision
            .legitimate_shrink
            .expect("B294 must be recognized as a legitimate shrink");
        assert_eq!(shrink.dropped, ["symbol:registry_digest"]);
    }

    #[test]
    fn r77_b296_replay_accepts_symbols_from_transitive_recorded_dependencies() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest_dir.ancestors().nth(3).unwrap();
        if !gitx::branch_exists(root, "main") {
            return;
        }
        let ledger = read_ledger(&root.join("coordination/rounds/r77/events.jsonl")).unwrap();
        let card = card::load(root, "r77", "B296").unwrap();
        let task_payloads = ledger
            .events
            .iter()
            .filter(|event| {
                event.kind == "SeedOracleVerified" && event.task_id.as_deref() == Some("B296")
            })
            .filter_map(|event| event.payload.clone())
            .collect::<Vec<_>>();
        let evidence = recorded_upstream_public_symbols(
            root,
            "r77",
            &card,
            &ledger.events,
            ledger.bad_lines.is_empty(),
        );
        assert_eq!(card.meta.depends_on, ["B295"]);
        assert_eq!(
            evidence.by_task.keys().cloned().collect::<Vec<_>>(),
            vec!["B293".to_string(), "B294".to_string(), "B295".to_string()],
            "the authenticated dependency closure must include B295 -> B294 -> B293"
        );
        assert!(
            evidence
                .by_task
                .get("B294")
                .is_some_and(|symbols| symbols
                    .contains("symbol:orch_host::harness::ENVELOPE_KEYS")),
            "B294 recorded evidence must explain ENVELOPE_KEYS: {:?}",
            evidence.by_task
        );
        assert!(
            evidence
                .by_task
                .get("B295")
                .is_some_and(|symbols| symbols
                    .contains("symbol:orch_host::harness::TerminalState")),
            "B295 recorded evidence must explain TerminalState: {:?}",
            evidence.by_task
        );

        let replay = CompileRedIdentity {
            dialect: "rustc".to_string(),
            diagnostics: vec![
                CompileDiagnostic {
                    code: "E0277".to_string(),
                    keys: vec![
                        "message:the trait bound `*const _: AsRef<Path>` is not satisfied"
                            .to_string(),
                    ],
                },
                CompileDiagnostic {
                    code: "E0425".to_string(),
                    keys: vec![
                        "symbol:wake".to_string(),
                        "symbol:write_nongate_receipt".to_string(),
                    ],
                },
                CompileDiagnostic {
                    code: "E0583".to_string(),
                    keys: vec!["symbol:agy_admission_and_derived_receipts_support".to_string()],
                },
            ],
        };
        let upstream_symbols = evidence.symbols();
        let decision =
            compile_oracle_replay_match(&task_payloads, None, &replay, &upstream_symbols).unwrap();
        let shrink = decision
            .legitimate_shrink
            .expect("B296 must recognize its recorded dependency closure");
        assert_eq!(
            shrink.dropped,
            [
                "symbol:ENVELOPE_KEYS",
                "symbol:orch_host::harness",
                "symbol:orch_host::harness::TerminalState",
            ]
        );
        assert_eq!(
            evidence.tasks_explaining(
                &shrink.dropped,
                &v2_compile_identity(task_payloads.last().unwrap()).unwrap(),
            ),
            ["B294", "B295"]
        );
    }

    #[test]
    fn observation_adds_identity_without_changing_measured_json() {
        let source = "#[test]\nfn missing_api_contract() {}\n".to_string();
        let observation = measure_cargo_observation(
            "error[E0432]: unresolved import `crate::missing_api`\n",
            101,
            &[source],
            &["orch/crates/orch-host/tests/seed_0.rs".into()],
        )
        .unwrap();
        let observation_json = serde_json::to_value(&observation).unwrap();
        assert!(observation_json.get("compileIdentity").is_some());
        let measured_json = serde_json::to_value(&observation.measured).unwrap();
        assert!(measured_json.get("compileIdentity").is_none());
    }

    #[test]
    fn generic_cargo_error_is_not_a_compile_observation() {
        let source = "#[test]\nfn contract() {}\n".to_string();
        for log in [
            "error: failed to run custom build command\n",
            "clang: error: unknown argument\n",
            "error: could not compile `orch-host`\n",
        ] {
            assert!(measure_cargo_observation(log, 101, std::slice::from_ref(&source),
                &["orch/crates/orch-host/tests/seed_0.rs".into()]).is_err());
        }
    }

    #[test]
    fn cargo_compile_red_builds_compile_measurement() {
        let source = "#[test]\nfn missing_api_contract() {}\n".to_string();
        let log = "Compiling seeded-test\nerror[E0432]: unresolved import `missing_api`\n";
        let measured = measure_cargo(log, 101, &[source]).unwrap();
        assert_eq!((measured.file_failed, measured.file_passed), (1, 0));
        assert_eq!(
            (measured.total_failed, measured.total_passed, measured.total),
            (1, 0, 1)
        );
        assert_eq!(measured.red_form.as_deref(), Some("compile"));
        assert_eq!(
            measured.failed_cases,
            vec!["<compile-red>: error[E0432]: unresolved import `missing_api`"]
        );
    }

    #[test]
    fn cargo_assertion_red_marks_assertion_form() {
        let source = "#[test]\nfn rejects_bad_value() {}\n".to_string();
        let log = "\
Running tests/seed_0.rs (orch/target/debug/deps/seed_0-0123456789abcdef)\n\
running 1 test\n\
test oracle::tests::rejects_bad_value ... FAILED\n\
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let measured = measure_cargo(log, 101, &[source]).unwrap();
        assert_eq!((measured.file_failed, measured.file_passed), (1, 0));
        assert_eq!(measured.red_form.as_deref(), Some("assertion"));
    }

    #[test]
    fn report_claim_error_discloses_claim_and_file_counts() {
        let claim = SuiteCounts {
            failed: 2,
            passed: 1,
            total: 3,
        };
        let measured = Measured {
            file_failed: 3,
            file_passed: 0,
            total_failed: 3,
            total_passed: 184,
            total: 187,
            failed_cases: Vec::new(),
            red_form: Some("assertion".to_string()),
        };
        let err = report_claim_ok(&claim, &measured).unwrap_err();
        assert!(err.contains("2f/1p"), "缺声称值: {err}");
        assert!(err.contains("3f/0p"), "缺文件内实测值: {err}");
        assert!(err.contains("文件内"), "未标明计数口径: {err}");
    }

    #[test]
    fn binding_without_oracle_defaults_to_vitest() {
        let b: crate::binding::Binding =
            serde_yaml::from_str("commands: {}\nunknownTopLevelField: tolerated\n").unwrap();
        assert_eq!(b.oracle.dialect, "vitest");
        assert!(b.project.ecosystems.is_empty());
    }

    #[test]
    fn narrative_mention_is_not_a_count_line() {
        // E13（r8/B12 误杀实测）：REPORT §3 的叙述句「无任何 `test result:` 行」自身含子串，
        // 但无真实数字——不得被当成 0f/0p 计数行
        let narrative = "在种子提交态执行 cargo test，exit 101；无任何 `test result:` 行，符合 redForm: compile。\n";
        assert!(parse_cargo_suite_counts(narrative).is_none());
        // 真计数行混在叙述里仍要被解析
        let mixed = "说明文字\ntest result: ok. 3 passed; 2 failed; 0 ignored\n";
        let s = parse_cargo_suite_counts(mixed).unwrap();
        assert_eq!((s.failed, s.passed), (2, 3));
    }

    #[test]
    fn red_replay_gate_empty_errors_with_seeded_keyword() {
        // 空列表 → Err，文案含 "seeded"（seeded 任务必须至少一个快门）
        let err = red_replay_gate(&[]).unwrap_err();
        assert!(err.contains("seeded"), "err should mention seeded: {err}");
    }

    #[test]
    fn red_replay_gate_takes_first_preserving_card_order() {
        // 多门取首（卡内顺序即优先级，非字典序）
        let gates = vec!["zeta".to_string(), "alpha".to_string(), "mid".to_string()];
        assert_eq!(red_replay_gate(&gates).unwrap(), "zeta");
        // 单门直通
        assert_eq!(
            red_replay_gate(&["testFast".to_string()]).unwrap(),
            "testFast"
        );
    }

    #[test]
    fn oracle_boundary_scopes_runner_artifacts_before_spawn() {
        let root = crate::util::test_scratch_dir("b272-oracle-scoped-gate");
        let log_dir = root.join("coordination/runtime/logs");
        fs::create_dir_all(&log_dir).unwrap();
        let spec = binding::CommandSpec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                ": > \"$ORCH_GATE_FIXTURE_REGISTRY\"; echo oracle-green".into(),
            ],
            timeout_seconds: 30,
            trial_timeout_seconds: None,
            approval: None,
        };
        let scoped_tag = gate::phase_scoped_log_tag(
            "r71",
            "B272",
            "pre-attempt",
            gate::GatePhase::RedReplay,
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        );
        let result = gate::run_gate("testFast", &spec, &root, &log_dir, &scoped_tag).unwrap();
        assert_eq!(result.exit_code, 0);

        let stem = "B272-round-r71-pre-attempt-red-replay-01ARZ3NDEKTSV4RRFFQ69G5FAV-gate-testFast";
        for extension in ["log", "hb", "fixtures"] {
            assert!(
                log_dir.join(format!("{stem}.{extension}")).is_file(),
                "oracle runner sibling missing: {extension}"
            );
        }
        assert!(!log_dir.join("B272-gate-testFast.log").exists());
    }

    #[test]
    fn schema3_relocation_requires_explicit_evolvable_policy_and_provenance() {
        let digest = "a".repeat(64);
        let opened = ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"purpose": "fixture", "contractSchemaVersion": 3}),
        );
        let oracle = ledger::event(
            "SeedOracleVerified",
            "planner",
            Some("B319"),
            Some("r83"),
            serde_json::json!({
                "oracleSchemaVersion": 2,
                "expectedRed": "fixture-red",
                "expectedRedProof": {"kind": "fixture"},
                "seeds": [{"target": "tests/contract.rs", "sha256": digest}]
                ,"irRevision": 1,
                "measured": {"fixture": true}
            }),
        );
        let validation_digest = "b".repeat(64);
        let validated = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r83"),
            crate::plan::task_validated_payload(1, &validation_digest),
        );
        let signed = ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some("r83"),
            crate::plan::plan_signed_off_payload("fixture", 1, &validation_digest).unwrap(),
        );
        let relocated = ledger::event(
            "SeedRelocated",
            "runtime:orch",
            Some("B319"),
            Some("r83"),
            serde_json::json!({
                "target": "tests/contract.rs", "sha256": digest,
                "cmp": "identical", "freezePolicy": "evolvable",
                "irRevision": 1, "seedOracleEventId": oracle.event_id.clone()
            }),
        );
        let recorded = ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B319"),
            Some("r83"),
            serde_json::json!({"postMergeGates": "all-green"}),
        );
        let events = vec![opened, validated, oracle, signed, relocated, recorded];
        validate_v3_relocation_history(
            &events,
            "r83",
            "B319",
            1,
            &validation_digest,
            true,
            true,
        )
        .unwrap();
        for replacement in [serde_json::Value::Null, serde_json::json!("permanent"), serde_json::json!("unknown")]
        {
            let mut mutated = events.clone();
            let payload = mutated[4].payload.as_mut().unwrap();
            if replacement.is_null() {
                payload.as_object_mut().unwrap().remove("freezePolicy");
            } else {
                payload["freezePolicy"] = replacement;
            }
            assert!(
                validate_v3_relocation_history(
                    &mutated,
                    "r83",
                    "B319",
                    1,
                    &validation_digest,
                    true,
                    true,
                )
                .is_err()
            );
        }
        let mut missing_oracle = events.clone();
        missing_oracle.remove(2);
        assert!(
            validate_v3_relocation_history(
                &missing_oracle,
                "r83",
                "B319",
                1,
                &validation_digest,
                true,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn schema3_same_revision_reuses_relocation_across_attempts() {
        let digest = "a".repeat(64);
        let validation_digest = "b".repeat(64);
        let mut events = vec![
            ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some("r83"),
                serde_json::json!({"purpose": "fixture", "contractSchemaVersion": 3}),
            ),
            ledger::event(
                "TaskValidated",
                "runtime:orch",
                None,
                Some("r83"),
                crate::plan::task_validated_payload(1, &validation_digest),
            ),
            ledger::event(
                "SeedOracleVerified",
                "planner",
                Some("B319"),
                Some("r83"),
                serde_json::json!({
                    "oracleSchemaVersion": 2,
                    "expectedRed": "fixture-red",
                    "expectedRedProof": {"kind": "fixture"},
                    "seeds": [{"target": "tests/contract.rs", "sha256": digest}],
                    "irRevision": 1,
                    "measured": {"fixture": true}
                }),
            ),
            ledger::event(
                "PlanSignedOff",
                "user",
                None,
                Some("r83"),
                crate::plan::plan_signed_off_payload("fixture", 1, &validation_digest).unwrap(),
            ),
        ];
        let seed_digests = vec![("tests/contract.rs".to_string(), digest)];
        let first = v3_seed_relocation_batch(&events, "r83", "B319", &seed_digests).unwrap();
        assert_eq!(first.len(), 1);
        events.extend(first);
        events.push(ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("B319"),
            Some("r83"),
            serde_json::json!({"verdict": "FAIL", "irRevision": 1}),
        ));
        assert!(
            v3_seed_relocation_batch(&events, "r83", "B319", &seed_digests)
                .unwrap()
                .is_empty()
        );
        events.push(ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B319"),
            Some("r83"),
            serde_json::json!({"postMergeGates": "all-green"}),
        ));
        validate_v3_relocation_history(
            &events,
            "r83",
            "B319",
            1,
            &validation_digest,
            true,
            true,
        )
        .unwrap();
    }

    #[test]
    fn schema3_unrelated_replan_keeps_recorded_task_oracle_epoch() {
        let digest_a = "a".repeat(64);
        let digest_b1 = "b".repeat(64);
        let digest_b2 = "c".repeat(64);
        let validation_1 = "d".repeat(64);
        let validation_2 = "e".repeat(64);
        let mut events = vec![
            ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some("r83"),
                serde_json::json!({"purpose": "fixture", "contractSchemaVersion": 3}),
            ),
            ledger::event(
                "TaskValidated",
                "runtime:orch",
                None,
                Some("r83"),
                crate::plan::task_validated_payload(1, &validation_1),
            ),
            ledger::event(
                "SeedOracleVerified",
                "planner",
                Some("B319"),
                Some("r83"),
                serde_json::json!({
                    "oracleSchemaVersion": 2, "expectedRed": "red-a",
                    "expectedRedProof": {"kind": "fixture"},
                    "seeds": [{"target": "tests/a.rs", "sha256": digest_a}],
                    "irRevision": 1, "measured": {"fixture": true}
                }),
            ),
            ledger::event(
                "SeedOracleVerified",
                "planner",
                Some("B320"),
                Some("r83"),
                serde_json::json!({
                    "oracleSchemaVersion": 2, "expectedRed": "red-b1",
                    "expectedRedProof": {"kind": "fixture"},
                    "seeds": [{"target": "tests/b.rs", "sha256": digest_b1}],
                    "irRevision": 1, "measured": {"fixture": true}
                }),
            ),
            ledger::event(
                "PlanSignedOff",
                "user",
                None,
                Some("r83"),
                crate::plan::plan_signed_off_payload("fixture", 1, &validation_1).unwrap(),
            ),
        ];
        let a_batch = v3_seed_relocation_batch(
            &events,
            "r83",
            "B319",
            &[("tests/a.rs".to_string(), digest_a)],
        )
        .unwrap();
        events.extend(a_batch);
        events.push(ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B319"),
            Some("r83"),
            serde_json::json!({"postMergeGates": "all-green"}),
        ));
        events.push(ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r83"),
            crate::plan::task_validated_payload_with_reverify(
                2,
                &validation_2,
                &["B320".to_string()],
            ),
        ));
        events.push(ledger::event(
            "SeedOracleVerified",
            "planner",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "oracleSchemaVersion": 2, "expectedRed": "red-b2",
                "expectedRedProof": {"kind": "fixture"},
                "seeds": [{"target": "tests/b.rs", "sha256": digest_b2}],
                "irRevision": 2, "measured": {"fixture": true}
            }),
        ));
        events.push(ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some("r83"),
            crate::plan::plan_signed_off_payload("fixture", 2, &validation_2).unwrap(),
        ));
        let b_batch = v3_seed_relocation_batch(
            &events,
            "r83",
            "B320",
            &[("tests/b.rs".to_string(), digest_b2)],
        )
        .unwrap();
        events.extend(b_batch);
        events.push(ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({"postMergeGates": "all-green"}),
        ));

        validate_v3_relocation_history(
            &events,
            "r83",
            "B319",
            1,
            &validation_1,
            true,
            true,
        )
        .unwrap();
        validate_v3_relocation_history(
            &events,
            "r83",
            "B320",
            2,
            &validation_2,
            true,
            true,
        )
        .unwrap();
    }

    #[test]
    fn schema3_unsigned_oracle_epoch_can_be_superseded_or_reused() {
        let digest_1 = "a".repeat(64);
        let digest_2 = "b".repeat(64);
        let validation_1 = "c".repeat(64);
        let validation_2 = "d".repeat(64);
        let opened = ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"purpose": "fixture", "contractSchemaVersion": 3}),
        );
        let validated_1 = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r83"),
            crate::plan::task_validated_payload(1, &validation_1),
        );
        let oracle_1 = ledger::event(
            "SeedOracleVerified",
            "planner",
            Some("B319"),
            Some("r83"),
            serde_json::json!({
                "oracleSchemaVersion": 2, "expectedRed": "red-1",
                "expectedRedProof": {"kind": "fixture"},
                "seeds": [{"target": "tests/a.rs", "sha256": digest_1}],
                "irRevision": 1, "measured": {"fixture": true}
            }),
        );
        let validated_2_self = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r83"),
            crate::plan::task_validated_payload_with_reverify(
                2,
                &validation_2,
                &["B319".to_string()],
            ),
        );
        let oracle_2 = ledger::event(
            "SeedOracleVerified",
            "planner",
            Some("B319"),
            Some("r83"),
            serde_json::json!({
                "oracleSchemaVersion": 2, "expectedRed": "red-2",
                "expectedRedProof": {"kind": "fixture"},
                "seeds": [{"target": "tests/a.rs", "sha256": digest_2}],
                "irRevision": 2, "measured": {"fixture": true}
            }),
        );
        let signed_2 = ledger::event(
            "PlanSignedOff",
            "user",
            None,
            Some("r83"),
            crate::plan::plan_signed_off_payload("fixture", 2, &validation_2).unwrap(),
        );
        let self_reverified = vec![
            opened.clone(),
            validated_1.clone(),
            oracle_1.clone(),
            validated_2_self,
            oracle_2.clone(),
            signed_2.clone(),
        ];
        let self_batch = v3_seed_relocation_batch(
            &self_reverified,
            "r83",
            "B319",
            &[("tests/a.rs".to_string(), digest_2)],
        )
        .unwrap();
        assert_eq!(self_batch.len(), 1);
        assert_eq!(
            self_batch[0].payload.as_ref().unwrap()["seedOracleEventId"],
            oracle_2.event_id
        );

        let validated_2_other = ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r83"),
            crate::plan::task_validated_payload_with_reverify(
                2,
                &validation_2,
                &["B320".to_string()],
            ),
        );
        let reused = vec![
            opened,
            validated_1,
            oracle_1.clone(),
            validated_2_other,
            signed_2,
        ];
        let reused_batch = v3_seed_relocation_batch(
            &reused,
            "r83",
            "B319",
            &[("tests/a.rs".to_string(), digest_1)],
        )
        .unwrap();
        assert_eq!(reused_batch.len(), 1);
        assert_eq!(
            reused_batch[0].payload.as_ref().unwrap()["irRevision"],
            2
        );
        assert_eq!(
            reused_batch[0].payload.as_ref().unwrap()["seedOracleEventId"],
            oracle_1.event_id
        );
    }

    #[test]
    fn retired_live_descriptor_recovers_real_r82_and_missing_closed_ir_fails() {
        let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf();
        let scratch = crate::util::test_scratch_dir("retired-live-baseline-r82");
        let run = |root: &Path, args: &[&str]| {
            let output = Command::new("git")
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
        };
        let output = Command::new("git")
            .args(["clone", "-q", "--shared", "--no-checkout"])
            .arg(&source_root)
            .arg(&scratch)
            .output()
            .unwrap();
        assert!(output.status.success(), "clone failed: {}", String::from_utf8_lossy(&output.stderr));
        run(&scratch, &["read-tree", "HEAD"]);
        fs::create_dir_all(scratch.join("coordination")).unwrap();
        fs::copy(
            source_root.join("coordination/PROJECT-BINDING.yaml"),
            scratch.join("coordination/PROJECT-BINDING.yaml"),
        )
        .unwrap();
        run(&scratch, &["add", "coordination/PROJECT-BINDING.yaml"]);
        let staged = Command::new("git")
            .arg("-C")
            .arg(&scratch)
            .args(["diff", "--cached", "--quiet"])
            .output()
            .unwrap();
        match staged.status.code() {
            // Once the migration binding itself is the checked-in HEAD, the
            // clone already represents the retired-descriptor state. Before
            // landing, copying the working binding creates the same fixture
            // through one explicit transition commit.
            Some(0) => {}
            Some(1) => run(
                &scratch,
                &[
                    "-c",
                    "user.name=orch-test",
                    "-c",
                    "user.email=orch@test.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "fixture: retire live descriptor",
                ],
            ),
            _ => panic!(
                "git staged-diff probe failed: {}",
                String::from_utf8_lossy(&staged.stderr)
            ),
        }
        let main = gitx::rev_parse(&scratch, "HEAD").unwrap();
        let (anchors, events) = load_signed_baseline(&scratch, &main).unwrap();
        assert!(!anchors.is_empty(), "r82 authenticated descriptor must recover anchors");
        assert!(events.iter().any(|event| {
            event.kind == "RoundClosed" && event.round.as_deref() == Some("r82")
        }));

        run(
            &scratch,
            &["rm", "--cached", "coordination/rounds/r82/ROUND-IR.yaml"],
        );
        run(
            &scratch,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                "fixture: remove latest closed ir",
            ],
        );
        let broken = gitx::rev_parse(&scratch, "HEAD").unwrap();
        assert!(load_signed_baseline(&scratch, &broken).is_err());
        fs::remove_dir_all(scratch).ok();
    }
}
