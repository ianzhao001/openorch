//! Optional project-wide storage budget and preserved, explicitly authorized retirement.
//! Accounting includes archives and Git; retirement never invents lifecycle events.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions}, io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf}, process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const POLICY: &str = "coordination/STORAGE-POLICY.yaml";
const LATCH: &str = "coordination/runtime/storage-maintenance/policy-enabled.json";
const RETIREMENTS: &str = "coordination/runtime/storage-maintenance/retirements";
const ARCHIVES: &str = "coordination/runtime/site-archives";
const CLOSE_RESERVE: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Policy {
    schema_version: u32,
    max_allocated_bytes: u64,
    keep_recent_rounds: u32,
    #[serde(default)]
    persistent_paths: BTreeMap<String, String>,
    #[serde(default)]
    round_custody: Vec<RoundCustody>,
    #[serde(default)]
    legacy_collect_source_custody: Vec<LegacyCollectSourceCustody>,
    #[serde(default)]
    legacy_failed_collection_custody: Vec<LegacyFailedCollectionCustody>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoundCustody {
    path: String,
    round: String,
    device: u64,
    inode: u64,
    head: String,
    evidence_path: String,
    evidence_sha256: String,
}
// A present-day retention declaration, not a backfilled lease or native terminal.
// Its evidence is an actual ended managed diagnostic at this source/HEAD.
fn custody_sources(root: &Path, policy: &Policy, current: u32,
    inventory: &crate::reclaim::MaintenanceInventory, bound_entry: Option<(&Path, &Entry)>) -> Result<Vec<(PathBuf, String)>> {
    if policy.round_custody.is_empty() { return Ok(Vec::new()); }
    let diagnostics = crate::buildcache::diagnostic_cache_records(root)?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for c in &policy.round_custody {
        let path = checked(root, &c.path)?;
        let owner = round_number(&c.round)?;
        if !c.path.starts_with(".cowork-temp/") || Path::new(&c.path).components().count() != 2
            || c.path.contains("collect-source-") || policy.persistent_paths.contains_key(&c.path)
            || !seen.insert(c.path.clone()) || c.round != format!("r{owner}") || owner > current {
            bail!("round custody path/round invalid or conflicting");
        }
        if c.head.len() != 40 || !c.head.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("round custody HEAD invalid");
        }
        if let Some((_, entry)) = bound_entry.filter(|(logical, _)| *logical == path) {
            // Bind custody to the manifest's exact original source. The retirement path
            // checks live/quarantine bytes before effects, or the matching prior receipt
            // plus relocated identity/digest/backup on recovery. Do not demand a vanished
            // original path before that recovery branch can validate the physical object.
            let expected = entry.preservation_root.as_ref()
                .map_or((entry.device, entry.inode), |p|(p.device, p.inode));
            if expected != (c.device, c.inode) || entry.head.as_deref() != Some(c.head.as_str()) {
                bail!("round custody source does not bind the retirement manifest");
            }
        } else if identity(&path)? != (c.device, c.inode)
            || git(&path, &["rev-parse", "HEAD"])? != c.head
            || fs::canonicalize(git(&path, &["rev-parse", "--show-toplevel"])?)? != path {
            bail!("round custody source identity/HEAD changed");
        }
        if inventory.claims.iter().any(|(owner, subject)| overlaps(&path, subject)
            && owner.split('/').next() != Some(c.round.as_str())) {
            bail!("round custody conflicts with existing claim");
        }
        if !inventory.rounds.iter().any(|r|r.round == c.round && r.error.is_none() && closed(&r.events, &c.round)) {
            bail!("round custody owner ledger missing, open or ambiguous");
        }
        let evidence = checked(root, &c.evidence_path)?;
        let rel = Path::new(&c.evidence_path);
        if !rel.starts_with("coordination/runtime/diagnostic-cache") || rel.components().count() != 5
            || rel.file_name().is_none_or(|n| n != "record.json") {
            bail!("round custody evidence is not a managed diagnostic record");
        }
        let bytes = regular_bytes(&evidence, 1024 * 1024)?;
        if hex::encode(Sha256::digest(&bytes)) != c.evidence_sha256 {
            bail!("round custody evidence digest changed");
        }
        if !diagnostics.iter().any(|d|Some(d.evidence_dir.as_path()) == evidence.parent()
            && d.disposition == "removed" && d.command_exit.is_some()) {
            bail!("round custody evidence is not a verified removed diagnostic");
        }
        let v: Value = serde_json::from_slice(&bytes)?;
        if v["schemaVersion"] != 1 || v["root"] != json!(root) || v["cwd"] != json!(path)
            || v["head"] != c.head || v["phase"] != "removed" || !v["exitCode"].is_i64()
            || v["id"].as_str() != evidence.parent().and_then(Path::file_name).and_then(|n| n.to_str())
            || !v["purpose"].as_str().is_some_and(|p|p.starts_with(&format!("{} ",c.round))) {
            bail!("round custody evidence does not bind this diagnostic source/round");
        }
        result.push((path, c.round.clone()));
    }
    Ok(result)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyCollectSourceCustody {
    path: String,
    round: String,
    task: String,
    attempt: String,
    device: u64,
    inode: u64,
    head: String,
    tree: String,
    exit_receipt_path: String,
    exit_receipt_sha256: String,
    log_path: String,
    log_sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySourceIdentity {
    path: String,
    round: String,
    task: String,
    attempt: String,
    device: u64,
    inode: u64,
    head: String,
    tree: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyFailedCollectionCustody {
    source: LegacySourceIdentity,
    executing_event_id: String,
    released_event_id: String,
    completed_event_id: String,
    log_path: String,
    log_bytes: u64,
    log_sha256: String,
}
// The log association is declared now, not a fabricated historical producer receipt.
fn failed_collection_lineage(events: &[orch_core::EventRecord], c: &LegacyFailedCollectionCustody) -> Result<()> {
    let source = &c.source;
    let find = |id: &str, kind: &str| -> Result<(usize, &Value)> {
        let matches: Vec<_> = events.iter().enumerate().filter(|(_, e)| e.event_id == id).collect();
        if matches.len() != 1 { bail!("failed collection custody event absent or ambiguous"); }
        let (i, e) = matches[0];
        if e.kind != kind || e.actor != "runtime:orch" || e.round.as_deref() != Some(&source.round)
            || e.task_id.as_deref() != Some(&source.task) {
            bail!("failed collection custody event identity mismatch");
        }
        Ok((i, e.payload.as_ref().context("failed collection custody payload absent")?))
    };
    let (start, executing) = find(&c.executing_event_id, "ReportCollectExecuting")?;
    let (end, released) = find(&c.released_event_id, "ReportCollectReleased")?;
    let (success, completed) = find(&c.completed_event_id, "ReportCollectCompleted")?;
    if !(start < end && end < success) || released.get("outcomeUnknown").is_some_and(|v| v != false) {
        bail!("failed collection custody order or outcome unknown");
    }
    for key in ["actionId", "owner", "leaseGeneration", "attemptId", "agent", "baseSha", "goPath",
        "evidencePath", "evidenceSha256", "controlEpoch"] {
        if !executing[key].as_str().is_some_and(|s| !s.trim().is_empty()) || executing[key] != released[key] {
            bail!("failed collection custody execution lineage mismatch: {key}");
        }
    }
    for key in ["attemptNo", "evidenceLen"] {
        if !executing[key].as_u64().is_some_and(|n| n > 0) || executing[key] != released[key] {
            bail!("failed collection custody numeric lineage mismatch: {key}");
        }
    }
    let attempt_no = source.attempt.strip_prefix(&format!("{}-A", source.task))
        .context("failed collection custody attempt")?.parse::<u64>()?;
    if executing["attemptId"] != source.attempt || executing["attemptNo"] != attempt_no
        || executing["agent"] != "local"
        || executing["goPath"] != format!("coordination/rounds/{}/dispatch/local/GO-{}.md", source.round, source.attempt)
        || completed["branchSha"] != source.head {
        bail!("failed collection custody task/attempt/candidate mismatch");
    }
    for (key, length) in [("baseSha", 40), ("evidenceSha256", 64), ("actionId", 64)] {
        if !executing[key].as_str().is_some_and(|s| s.len() == length && s.bytes().all(|b| b.is_ascii_hexdigit())) {
            bail!("failed collection custody invalid digest: {key}");
        }
    }
    for key in ["owner", "leaseGeneration"] {
        if !completed[key].as_str().is_some_and(|s| !s.trim().is_empty()) || completed[key] == executing[key] {
            bail!("failed collection custody contradictory completed generation: {key}");
        }
    }
    // A later completed action binds REPORT identity to HEAD, not the earlier action's success.
    for key in ["actionId", "attemptId", "attemptNo", "agent", "baseSha", "goPath", "evidencePath",
        "evidenceSha256", "evidenceLen", "controlEpoch"] {
        if completed[key] != executing[key] { bail!("failed collection custody completed REPORT lineage mismatch: {key}"); }
    }
    let same_generation = |e: &orch_core::EventRecord| e.payload.as_ref().is_some_and(|p|
        p["actionId"] == executing["actionId"] && p["leaseGeneration"] == executing["leaseGeneration"]);
    if events.iter().filter(|e| same_generation(e) && e.kind == "ReportCollectExecuting").count() != 1
        || events.iter().filter(|e| same_generation(e) && e.kind == "ReportCollectReleased").count() != 1
        || events.iter().any(|e| same_generation(e) && matches!(e.kind.as_str(), "ReportCollectCompleted" | "ReportCollectExecuted"))
        || events[start + 1..end].iter().any(|e| e.kind == "ReportCollectExecuting" && e.task_id.as_deref() == Some(&source.task)) {
        bail!("failed collection custody contradictory or overlapping execution");
    }
    if !events[start + 1..end].iter().any(|e| e.kind == "GateExecuted" && e.actor == "runtime:orch"
        && e.round.as_deref() == Some(&source.round) && e.task_id.as_deref() == Some(&source.task)
        && e.payload.as_ref().is_some_and(|p| matches!(p["phase"].as_str(), Some("collect" | "trial"))
            && p["exitCode"].as_i64().is_some_and(|n| n != 0))) {
        bail!("failed collection custody missing actual failed gate");
    }
    Ok(())
}

fn legacy_source_identity(root: &Path, policy: &Policy, current: u32,
    inventory: &crate::reclaim::MaintenanceInventory, seen: &mut BTreeSet<String>,
    c: &LegacySourceIdentity) -> Result<PathBuf> {
    let path = checked(root, &c.path)?;
    let owner = round_number(&c.round)?;
    if !c.path.starts_with(".cowork-temp/collect-source-")
        || Path::new(&c.path).components().count() != 2
        || !seen.insert(c.path.clone()) || c.round != format!("r{owner}") || owner > current
        || policy.persistent_paths.contains_key(&c.path)
        || policy.round_custody.iter().any(|d| d.path == c.path) {
        bail!("legacy collect custody path/round invalid or conflicting");
    }
    let attempt_number = c.attempt.strip_prefix(&format!("{}-A", c.task));
    if c.task.is_empty() || !c.task.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || !attempt_number.is_some_and(|s| s.len() >= 4 && s.bytes().all(|b| b.is_ascii_digit()) && s.parse::<u32>().is_ok_and(|n| n > 0))
        || [&c.head, &c.tree].iter().any(|s| s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit())) {
        bail!("legacy collect custody task/attempt/Git identity invalid");
    }
    if identity(&path)? != (c.device, c.inode)
        || git(&path, &["rev-parse", "HEAD"])? != c.head
        || git(&path, &["rev-parse", "HEAD^{tree}"])? != c.tree
        || fs::canonicalize(git(&path, &["rev-parse", "--show-toplevel"])?)? != path
        || fs::canonicalize(path.join(git(&path, &["rev-parse", "--git-common-dir"])?))? != path.join(".git")
        || !git(&path, &["status", "--porcelain", "--untracked-files=all"] )?.is_empty() {
        bail!("legacy collect custody source identity/HEAD/tree/clean root changed");
    }
    if inventory.claims.iter().any(|(_, subject)| overlaps(&path, subject)) {
        bail!("legacy collect custody conflicts with existing claim");
    }
    for receipt in receipt_paths(root)? {
        if overlaps(&path, &receipt_subject(root, &read_receipt(root, &receipt)?)?) {
            bail!("legacy collect custody conflicts with producer receipt");
        }
    }
    Ok(path)
}

// Classification returns verified bindings; it alone grants no deletion permission.
fn validate_launcher_custody(root: &Path, policy: &Policy, current: u32,
    inventory: &crate::reclaim::MaintenanceInventory, seen: &mut BTreeSet<String>,
    c: &LegacyCollectSourceCustody) -> Result<ValidatedLegacyCustody> {
    let source = LegacySourceIdentity { path: c.path.clone(), round: c.round.clone(), task: c.task.clone(),
        attempt: c.attempt.clone(), device: c.device, inode: c.inode, head: c.head.clone(), tree: c.tree.clone() };
    let path = legacy_source_identity(root, policy, current, inventory, seen, &source)?;
    let round = inventory.rounds.iter().find(|r| r.round == c.round && r.error.is_none())
        .context("legacy collect custody owner round absent or ambiguous")?;
    if !round.events.iter().any(|e| e.kind == "ReportCollectCompleted" && e.actor == "runtime:orch"
        && e.round.as_deref() == Some(&c.round) && e.task_id.as_deref() == Some(&c.task)
        && e.payload.as_ref().is_some_and(|v| v["attemptId"] == c.attempt && v["branchSha"] == c.head)) {
        bail!("legacy collect custody has no matching collected candidate");
    }
    let proof_prefix = format!("coordination/runtime/logs/{}/", c.round);
    if !c.exit_receipt_path.starts_with(&proof_prefix) || !c.log_path.starts_with(&proof_prefix) {
        bail!("legacy collect custody evidence outside owner round logs");
    }
    let receipt = regular_bytes(&checked(root, &c.exit_receipt_path)?, 1024 * 1024)?;
    let log_path = checked(root, &c.log_path)?;
    let log = regular_bytes(&log_path, 16 * 1024 * 1024)?;
    if hex::encode(Sha256::digest(&receipt)) != c.exit_receipt_sha256
        || hex::encode(Sha256::digest(&log)) != c.log_sha256 {
        bail!("legacy collect custody evidence digest changed");
    }
    let v: Value = serde_json::from_slice(&receipt)?;
    let pid = v["pid"].as_u64().context("legacy collect custody producer PID absent")?;
    if v["task"] != c.task || v["attempt"] != c.attempt || v["candidateHead"] != c.head
        || !v["exitCode"].as_i64().is_some_and(|code| (1..=255).contains(&code))
        || v["log"] != json!(log_path) || v["logSha256"] != c.log_sha256
        || !v["finishedAt"].as_str().is_some_and(|s| !s.is_empty())
        || !Path::new(&c.path).file_name().and_then(|n| n.to_str())
            .is_some_and(|n| n.strip_prefix(&format!("collect-source-{pid}-"))
                .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())))
        || !String::from_utf8(log)?.lines().any(|line| line == format!(
            "collect snapshot retained for incomplete/failed collection at {}", path.display())) {
        bail!("legacy collect custody terminal/log does not bind the source");
    }
    pid_absent(pid)?;
    quiet(&path)?;
    Ok(ValidatedLegacyCustody { source, custody_kind: LegacyCustodyKind::SealLauncher,
        custody_sha256: hex::encode(Sha256::digest(serde_json::to_vec(c)?)) })
}
fn validate_failed_custody(root: &Path, policy: &Policy, current: u32,
    inventory: &crate::reclaim::MaintenanceInventory, seen: &mut BTreeSet<String>,
    c: &LegacyFailedCollectionCustody) -> Result<ValidatedLegacyCustody> {
    let source = &c.source;
    let path = legacy_source_identity(root, policy, current, inventory, seen, source)?;
    let round = inventory.rounds.iter().find(|r| r.round == source.round && r.error.is_none())
        .context("failed collection custody owner round absent or ambiguous")?;
    failed_collection_lineage(&round.events, c)?;
    if !c.log_path.starts_with(&format!("coordination/runtime/logs/{}/", source.round)) {
        bail!("failed collection custody log outside owner round");
    }
    let bytes = regular_bytes(&checked(root, &c.log_path)?, 16 * 1024 * 1024)?;
    if bytes.len() as u64 != c.log_bytes || hex::encode(Sha256::digest(&bytes)) != c.log_sha256 {
        bail!("failed collection custody log length/digest changed");
    }
    if !String::from_utf8(bytes)?.lines().any(|line| line == format!(
        "collect snapshot retained for incomplete/failed collection at {}", path.display())) {
        bail!("failed collection custody log does not name retained source");
    }
    // Released is not a native terminal. Observe current use; do not invent an old PID.
    quiet(&path)?;
    Ok(ValidatedLegacyCustody { source: source.clone(), custody_kind: LegacyCustodyKind::FailedCollection,
        custody_sha256: hex::encode(Sha256::digest(serde_json::to_vec(c)?)) })
}
fn legacy_collect_sources(root: &Path, policy: &Policy, current: u32,
    inventory: &crate::reclaim::MaintenanceInventory) -> Result<Vec<ValidatedLegacyCustody>> {
    let mut seen = BTreeSet::new(); let mut result = Vec::new();
    for c in &policy.legacy_collect_source_custody {
        result.push(validate_launcher_custody(root, policy, current, inventory, &mut seen, c)?);
    }
    for c in &policy.legacy_failed_collection_custody {
        result.push(validate_failed_custody(root, policy, current, inventory, &mut seen, c)?);
    }
    Ok(result)
}

fn checked(root: &Path, relative: &str) -> Result<PathBuf> {
    crate::reclaim::checked_storage_path(root, Path::new(relative))
}
fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn regular_bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() > limit {
        bail!("storage policy input is not a bounded regular file: {}", path.display());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit { bail!("storage policy input grew"); }
    Ok(bytes)
}
fn load(root: &Path) -> Result<Option<(Policy, String)>> {
    // Legacy callers may use an ordinary /var or /tmp alias. Do not add any
    // canonical-root constraint when this project has never enabled a policy.
    if !present(&root.join(POLICY))? && !present(&root.join(LATCH))? { return Ok(None); }
    crate::reclaim::validate_storage_root(root)?;
    let path = checked(root, POLICY)?;
    if !present(&path)? {
        if present(&checked(root, LATCH)?)? { bail!("storage policy missing after activation"); }
        return Ok(None);
    }
    let bytes = regular_bytes(&path, 65536)?;
    let policy: Policy = serde_yaml::from_slice(&bytes).context("storage policy malformed")?;
    if policy.schema_version != 1 || policy.max_allocated_bytes == 0
        || policy.keep_recent_rounds == 0 || policy.keep_recent_rounds > 1000 {
        bail!("storage policy version/limits invalid");
    }
    for (path, reason) in &policy.persistent_paths {
        checked(root, path)?;
        if !path.starts_with(".cowork-temp/") || path.contains("collect-source-")
            || Path::new(path).components().count() != 2 || reason.trim().is_empty() {
            bail!("storage policy persistent path must name a non-round workspace and reason");
        }
    }
    Ok(Some((policy, hex::encode(Sha256::digest(bytes)))))
}
fn round_number(s: &str) -> Result<u32> {
    s.strip_prefix('r').filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .context("storage policy needs r<number> round identity")?.parse()
        .context("storage policy round overflow")
}
fn identity(path: &Path) -> Result<(u64, u64)> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() { bail!("retirement requires exact directory identity"); }
    Ok((m.dev(), m.ino()))
}
fn allocated(path: &Path, seen: &mut BTreeSet<(u64, u64)>) -> Result<u64> {
    let m = fs::symlink_metadata(path)
        .with_context(|| format!("storage policy cannot measure {}", path.display()))?;
    if !seen.insert((m.dev(), m.ino())) { return Ok(0); }
    let mut n = m.blocks().checked_mul(512).context("allocated byte overflow")?;
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            n = n.checked_add(allocated(&e?.path(), seen)?).context("allocated byte overflow")?;
        }
    }
    Ok(n)
}
fn overlaps(a: &Path, b: &Path) -> bool { a.starts_with(b) || b.starts_with(a) }
fn closed(events: &[orch_core::EventRecord], round: &str) -> bool {
    events.iter().any(|e| e.kind == "RoundClosed" && e.actor == "runtime:orch"
        && e.round.as_deref() == Some(round))
}
fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    if !present(path)? { return Ok(Vec::new()); }
    identity(path)?;
    let mut out = fs::read_dir(path)?.map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    out.sort();
    Ok(out)
}
fn receipt_paths(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(entries(&checked(root, ".cowork-temp")?)?.into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".receipt.json")))
        .collect())
}
fn read_receipt(root: &Path, path: &Path) -> Result<Value> {
    let v: Value = serde_json::from_slice(&regular_bytes(path, 1024 * 1024)?)?;
    if v["schemaVersion"] != 1 || v["projectRoot"].as_str() != root.to_str() {
        bail!("storage policy snapshot receipt root/version mismatch");
    }
    Ok(v)
}
fn receipt_subject(root: &Path, v: &Value) -> Result<PathBuf> {
    let absolute = Path::new(v["path"].as_str().context("snapshot path absent")?);
    let relative = absolute.strip_prefix(root)?.to_str().context("snapshot path not UTF-8")?;
    if !relative.starts_with(".cowork-temp/collect-source-")
        || Path::new(relative).components().count() != 2 { bail!("snapshot receipt path outside namespace"); }
    checked(root, relative)
}

/// Read whole-project allocated bytes, retained rounds and unresolved scene ownership.
/// Uses lstat/inode dedup like du; symlinks are counted, never followed. APFS shared extents
/// are not exclusive physical ownership. Missing policy preserves legacy behavior.
/// Explicit legacy collect-source custody verifies current source and failed-operation evidence
/// plus present quiescence; it only classifies retention and never grants retirement authority.
/// Failed-collection declarations instead bind real action lineage and associated original logs;
/// neither a released action nor later success is treated as historical producer termination.
/// Returned legacyCustody descriptors expose canonical typed proof digests for explicit administration;
/// they do not themselves permit deletion.
pub fn assess(root: &Path, round: &str) -> Result<Option<Value>> {
    let Some((policy, digest)) = load(root)? else { return Ok(None); };
    let n = round_number(round)?;
    let first = n.saturating_sub(policy.keep_recent_rounds - 1).max(1);
    let inventory = crate::reclaim::storage_inventory(root);
    if let Some(e) = &inventory.global_hold { bail!("storage policy ownership inventory: {e}"); }
    if !inventory.errors.is_empty() { bail!("storage policy incomplete round inventory"); }
    let mut expired = BTreeSet::new();
    let mut known = BTreeSet::new();
    for (owner, path) in &inventory.claims {
        if !present(path)? { continue; }
        let relative = path.strip_prefix(root)?.to_str().context("scene path not UTF-8")?;
        if !retirable(relative) { continue; }
        checked(root, relative)?;
        known.insert(path.clone());
        if round_number(owner.split('/').next().unwrap_or(""))? < first {
            expired.insert(relative.to_string());
        }
    }
    for p in receipt_paths(root)? {
        let v = read_receipt(root, &p)?;
        let path = receipt_subject(root, &v)?;
        if !present(&path)? { continue; }
        if let (Some(dev), Some(ino)) = (v["device"].as_u64(), v["inode"].as_u64()) {
            if identity(&path)? != (dev, ino) { bail!("storage policy snapshot identity changed"); }
        }
        known.insert(path.clone());
        let owner = v.pointer("/owner/round").and_then(Value::as_str);
        if owner.is_none() || round_number(owner.unwrap())? < first {
            expired.insert(path.strip_prefix(root)?.to_string_lossy().into_owned());
        }
    }
    for p in entries(&checked(root, RETIREMENTS)?)? {
        if p.extension().is_none_or(|e| e != "json") { continue; }
        let v: Value = serde_json::from_slice(&regular_bytes(&p,1024*1024)?)?;
        if v["schemaVersion"] != 1 { bail!("storage policy invalid retirement receipt"); }
        if v["phase"] != "removed" { continue; }
        if let Some(preserved) = v.pointer("/entry/preservationRoot").filter(|v| !v.is_null()) {
            let relative = preserved["path"].as_str().context("preserved source path absent")?;
            let path = checked(root, relative)?;
            if !present(&path)? { continue; }
            if preserved["device"].as_u64().zip(preserved["inode"].as_u64()) != Some(identity(&path)?) {
                bail!("storage policy preserved source identity changed");
            }
            known.insert(path);
            if round_number(v.pointer("/entry/round").and_then(Value::as_str).context("retirement owner absent")?)? < first {
                expired.insert(relative.to_string());
            }
        }
    }
    for (path, owner) in custody_sources(root, &policy, n, &inventory, None)? {
        known.insert(path.clone());
        if round_number(&owner)? < first {
            expired.insert(path.strip_prefix(root)?.to_string_lossy().into_owned());
        }
    }
    let legacy_custody = legacy_collect_sources(root, &policy, n, &inventory)?;
    for bound in &legacy_custody {
        let path = checked(root, &bound.source.path)?;
        known.insert(path);
        if round_number(&bound.source.round)? < first {
            expired.insert(bound.source.path.clone());
        }
    }
    let mut unknown = BTreeSet::new();
    for namespace in [".cowork-temp", ".worktrees"] {
        for path in entries(&checked(root, namespace)?)? {
            if fs::symlink_metadata(&path)?.is_file() { continue; }
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            if !known.iter().any(|p| path.starts_with(p))
                && !policy.persistent_paths.contains_key(&relative) { unknown.insert(relative); }
        }
    }
    for cache in crate::buildcache::diagnostic_cache_records(root)? {
        if present(&cache.path)? {
            unknown.insert(cache.path.strip_prefix(root)?.to_string_lossy().into_owned());
        }
    }
    let bytes = allocated(root, &mut BTreeSet::new())?;
    Ok(Some(json!({"projectAllocatedBytes": bytes, "maxAllocatedBytes": policy.max_allocated_bytes,
        "retainedRounds": (first..=n).map(|n|format!("r{n}")).collect::<Vec<_>>(),
        "expiredPaths": expired, "unclassifiedPaths": unknown, "policySha256": digest, "legacyCustody": legacy_custody,
        "policySatisfied": bytes <= policy.max_allocated_bytes && expired.is_empty() && unknown.is_empty(),
        "measurement": "allocated-inode-dedup; symlinks not followed; archives included"})))
}
/// Apply the same optional retention window to existing whole-worktree reapers.
/// A missing worktree leaves independent target reclamation available; bad policy/identity holds.
pub(crate) fn retained_worktree_reason(root: &Path, round: &str, worktree: &Path) -> Result<Option<String>> {
    let Some((policy,_)) = load(root)? else { return Ok(None); };
    let relative = worktree.strip_prefix(root)?.to_str().context("retention path not UTF-8")?;
    checked(root,relative)?;
    if !present(worktree)? { return Ok(None); }
    let current = round_number(&crate::current_round(root)?)?;
    let first = current.saturating_sub(policy.keep_recent_rounds-1).max(1);
    if round_number(round)? >= first {
        return Ok(Some(format!("storage policy retained round {round}; keep whole worktree, use ended-target cleanup separately")));
    }
    Ok(None)
}
/// Refuse close before DONE and append; force never bypasses policy. Reserve bounded metadata space.
pub(crate) fn guard_close(root: &Path) -> Result<Option<String>> {
    if load(root)?.is_none() { return Ok(None); }
    let round = crate::current_round(root)?;
    let v = assess(root, &round)?.context("storage policy disappeared")?;
    let bytes = v["projectAllocatedBytes"].as_u64().context("missing allocated bytes")?;
    let limit = v["maxAllocatedBytes"].as_u64().context("missing byte limit")?;
    if v["policySatisfied"] != true || bytes.checked_add(CLOSE_RESERVE).is_none_or(|n| n > limit) {
        bail!("storage policy refuses close (includes 16MiB metadata reserve): {v}");
    }
    Ok(v["policySha256"].as_str().map(str::to_owned))
}
pub(crate) fn activate(root: &Path) -> Result<()> {
    if let Some((_, digest)) = load(root)? {
        let path = checked(root, LATCH)?;
        if !present(&path)? {
            atomic_json(root, &path, &json!({"schemaVersion":1,"policyPath":POLICY,"initialPolicySha256":digest}))?;
        }
    }
    Ok(())
}
fn atomic_json(root: &Path, path: &Path, value: &Value) -> Result<()> {
    checked(root, path.strip_prefix(root)?.to_str().context("receipt path UTF-8")?)?;
    let parent = path.parent().context("receipt parent")?;
    fs::create_dir_all(parent)?;
    let before = identity(parent)?;
    let tmp = parent.join(format!(".write-{}", ulid::Ulid::new()));
    let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    checked(root, path.strip_prefix(root)?.to_str().context("receipt path UTF-8")?)?;
    if identity(parent)? != before { bail!("receipt parent replaced; preserved temporary receipt"); }
    fs::rename(tmp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Owner supplied by the production caller, not reconstructed from a clone's CURRENT.
#[derive(Clone, Serialize)]
#[serde(rename_all="camelCase")]
pub(crate) struct SnapshotOwner<'a> {
    pub round: &'a str, pub task_id: &'a str, pub attempt_id: &'a str, pub purpose: &'a str,
}
pub(crate) struct SnapshotReceipt { root: PathBuf, subject: PathBuf, path: PathBuf, value: Value }
impl SnapshotReceipt {
    pub(crate) fn new(root: &Path, subject: &Path, head: &str, owner: Option<SnapshotOwner<'_>>) -> Result<Self> {
        let path = subject.with_extension("receipt.json");
        if present(&path)? { bail!("snapshot receipt already exists"); }
        let value = json!({"schemaVersion":1,"projectRoot":root,"path":subject,"head":head,
            "owner":owner,"producerPid":std::process::id(),"createdAt":crate::util::now_rfc3339(),"phase":"prepared"});
        atomic_json(root, &path, &value)?;
        Ok(Self {root:root.into(),subject:subject.into(),path,value})
    }
    pub(crate) fn update(&mut self, phase: &str, head: Option<&str>) -> Result<()> {
        if self.value["device"].is_null() && present(&self.subject)? {
            let (dev, ino) = identity(&self.subject)?;
            self.value["device"] = json!(dev); self.value["inode"] = json!(ino);
        }
        self.value["phase"] = json!(phase);
        self.value["updatedAt"] = json!(crate::util::now_rfc3339());
        if let Some(head) = head { self.value["head"] = json!(head); }
        atomic_json(&self.root, &self.path, &self.value)
    }
}
impl Drop for SnapshotReceipt {
    fn drop(&mut self) {
        if self.value["phase"] != "removed" {
            if let Err(e) = self.update("retained", None) { eprintln!("snapshot receipt retained with update error: {e:#}"); }
        }
    }
}

fn cache_exclusions(excluded: &[String]) -> Result<()> {
    if excluded.len() > 1 || excluded.iter().any(|p| !matches!(p.as_str(), "orch/target" | "orch/target/debug")) {
        bail!("retirement exclusion must be the fixed Cargo target or its debug subtree");
    }
    Ok(())
}
fn digest_walk(base: &Path, path: &Path, excluded: &[String], hash: &mut Sha256) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let relative = path.strip_prefix(base)?;
    if excluded.iter().any(|p| relative.starts_with(p)) { return Ok(()); }
    let m = fs::symlink_metadata(path)?;
    // A Git fsmonitor socket is transient IPC, not restorable Git data. Its presence
    // still has to pass the independent current-use checks before physical retirement.
    if m.file_type().is_socket() && path.file_name().is_some_and(|n| n == "fsmonitor--daemon.ipc")
        && (relative.starts_with(".git") || base.file_name().is_some_and(|n| n == ".git")) {
        return Ok(());
    }
    let name = relative.as_os_str().as_encoded_bytes();
    hash.update((name.len() as u64).to_le_bytes()); hash.update(name);
    hash.update((m.mode() & 0o7777).to_le_bytes());
    if m.file_type().is_symlink() {
        hash.update(b"L"); hash.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
    } else if m.is_file() {
        hash.update(b"F"); hash.update(m.len().to_le_bytes());
        let mut file = fs::File::open(path)?;
        let mut buffer = [0u8; 65536];
        loop { let n = file.read(&mut buffer)?; if n == 0 { break; } hash.update(&buffer[..n]); }
    } else if m.is_dir() {
        hash.update(b"D");
        for child in entries(path)? { digest_walk(base, &child, excluded, hash)?; }
    } else if m.file_type().is_fifo() {
        // Historical test fixtures are named pipes, not streams to consume.
        hash.update(b"P");
    } else { bail!("unsupported preserved file type: {}", path.display()); }
    hash.update([0xff]);
    Ok(())
}
/// Hash path/type/mode/content/symlink bytes, including complete Git data, for preservation.
/// FIFO fixtures contribute path, type and mode only; their streams are never opened or read.
/// Only the fixed Cargo target/debug boundary may be excluded. Transient Git monitor sockets are not data.
/// This content recovery contract does not assert ACL, xattr, owner or hardlink-topology equality.
pub fn preservation_digest(path: &Path, excluded: &[String]) -> Result<String> {
    cache_exclusions(excluded)?;
    identity(path)?;
    let mut hash = Sha256::new();
    digest_walk(path, path, excluded, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}
fn data_inodes(path: &Path, base: &Path, excluded: &[String], out: &mut BTreeSet<(u64,u64)>) -> Result<()> {
    if excluded.iter().any(|p| path.strip_prefix(base).is_ok_and(|rel| rel.starts_with(p))) { return Ok(()); }
    let m = fs::symlink_metadata(path)?;
    if m.is_file() { out.insert((m.dev(), m.ino())); }
    if m.is_dir() { for child in entries(path)? { data_inodes(&child, base, excluded, out)?; } }
    Ok(())
}
fn independent_copy(source: &Path, backup: &Path, excluded: &[String]) -> Result<()> {
    if overlaps(source, backup) { bail!("preservation source and backup overlap"); }
    let mut a = BTreeSet::new(); let mut b = BTreeSet::new();
    data_inodes(source, source, excluded, &mut a)?;
    data_inodes(backup, backup, excluded, &mut b)?;
    if !a.is_disjoint(&b) { bail!("preservation backup shares mutable file inodes with source"); }
    Ok(())
}
#[derive(Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct Manifest {
    schema_version: u32, project_root: PathBuf, authorization: String, expires_at: u64, entries: Vec<Entry>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct Entry {
    path: String, round: String, device: u64, inode: u64, head: Option<String>, kind: String,
    source_digest: String, backup_path: String, excluded_caches: Vec<String>,
    #[serde(default)] git_preservation: Option<GitPreservation>,
    /// Present only to remove this source root's reproducible orch/target, retaining the source root.
    #[serde(default)] preservation_root: Option<PreservationRoot>,
    /// Relocate an intact directory to an on-project archive; never delete its nested fixture data.
    #[serde(default)] archive_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_debug_retirement: Option<LegacyDebugRetirement>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct PreservationRoot { path: String, device: u64, inode: u64 }
#[derive(Deserialize, Serialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct GitPreservation {
    common_backup: String, common_digest: String, private_backup: String,
    private_digest: String, restored_worktree: String,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum LegacyCustodyKind { SealLauncher, FailedCollection }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidatedLegacyCustody {
    source: LegacySourceIdentity,
    custody_kind: LegacyCustodyKind,
    custody_sha256: String,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyDebugRetirement {
    task_id: String,
    custody_kind: LegacyCustodyKind,
    custody_sha256: String,
    recorded_event_id: String,
    merge_executed_event_id: String,
    recorded_attempt_id: String,
    recorded_head: String,
}
fn legacy_debug_shape(entry: &Entry) -> Result<()> {
    if entry.legacy_debug_retirement.is_none() { return Ok(()); }
    let source = entry.preservation_root.as_ref().context("legacy debug requires preserved parent")?;
    if !source.path.starts_with(".cowork-temp/collect-source-") || Path::new(&source.path).components().count() != 2
        || entry.path != format!("{}/orch/target/debug", source.path)
        || entry.excluded_caches != ["orch/target/debug"] || entry.kind != "directory"
        || entry.archive_path.is_some() || entry.head.is_none() {
        bail!("legacy debug authority only covers exact preserved independent source debug");
    }
    Ok(())
}
fn ordinary_git_history(path: &Path) -> Result<()> {
    for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR", "GIT_INDEX_FILE", "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_REPLACE_REF_BASE", "GIT_SHALLOW_FILE"] {
        if std::env::var_os(key).is_some() { bail!("legacy debug requires unambiguous Git environment: {key}"); }
    }
    if git(path, &["rev-parse", "--is-shallow-repository"])? != "false"
        || !git(path, &["for-each-ref", "--format=%(refname)", "refs/replace"] )?.is_empty() {
        bail!("legacy debug refuses shallow or replacement history");
    }
    let common = PathBuf::from(git(path, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    if present(&common.join("info/grafts"))? { bail!("legacy debug refuses graft history"); }
    Ok(())
}
fn legacy_debug_authority(root: &Path, entry: &Entry, inv: &crate::reclaim::MaintenanceInventory) -> Result<String> {
    legacy_debug_shape(entry)?;
    let proof = entry.legacy_debug_retirement.as_ref().context("legacy debug explicit proof absent")?;
    let parent = entry.preservation_root.as_ref().context("legacy debug parent absent")?;
    let source = checked(root, &parent.path)?;
    ordinary_git_history(root)?;
    ordinary_git_history(&source)?;
    let main = git(root, &["rev-parse", "refs/heads/main"])?;
    let (policy, _) = load(root)?.context("legacy debug requires explicit custody policy")?;
    let current = round_number(&crate::current_round(root)?)?;
    let count = policy.legacy_collect_source_custody.iter().filter(|c| c.path == parent.path).count()
        + policy.legacy_failed_collection_custody.iter().filter(|c| c.source.path == parent.path).count();
    if count != 1 { bail!("legacy debug selected custody absent or ambiguous"); }
    let mut seen = BTreeSet::new();
    let bound = match proof.custody_kind {
        LegacyCustodyKind::SealLauncher => validate_launcher_custody(root, &policy, current, inv, &mut seen,
            policy.legacy_collect_source_custody.iter().find(|c| c.path == parent.path).context("legacy debug custody kind mismatch")?)?,
        LegacyCustodyKind::FailedCollection => validate_failed_custody(root, &policy, current, inv, &mut seen,
            policy.legacy_failed_collection_custody.iter().find(|c| c.source.path == parent.path).context("legacy debug custody kind mismatch")?)?,
    };
    if bound.custody_sha256 != proof.custody_sha256 || bound.source.task != proof.task_id
        || bound.source.round != entry.round || entry.head.as_deref() != Some(&bound.source.head)
        || (bound.source.device, bound.source.inode) != (parent.device, parent.inode) {
        bail!("legacy debug custody digest/task/source mismatch");
    }
    let owner = inv.rounds.iter().find(|r| r.round == entry.round && r.error.is_none())
        .context("legacy debug owner ledger absent or ambiguous")?;
    if crate::round::contract_schema_at_root(root, &entry.round)? != Some(3) {
        bail!("legacy debug requires real schema3 Recorded authority");
    }
    let authorization = crate::verify::validate_root_record_authorization(root, &entry.round, &proof.task_id, &owner.events)?;
    if !authorization.already_recorded || authorization.implementer_agent != "local"
        || authorization.attempt_id != proof.recorded_attempt_id || authorization.head_sha != proof.recorded_head {
        bail!("legacy debug requires exact last Recorded local attempt/head");
    }
    crate::close::validate_seal_postcondition(&owner.events, &entry.round, &proof.task_id, &authorization.attempt_id)?;
    let exact = |id: &str, kind: &str| -> Result<(usize, &orch_core::EventRecord)> {
        let rows: Vec<_> = owner.events.iter().enumerate().filter(|(_, e)| e.event_id == id).collect();
        if rows.len() != 1 { bail!("legacy debug lifecycle event absent or duplicate"); }
        let (i, e) = rows[0];
        if e.kind != kind || e.task_id.as_deref() != Some(&proof.task_id) || e.round.as_deref() != Some(&entry.round) {
            bail!("legacy debug lifecycle identity mismatch");
        }
        Ok((i, e))
    };
    let (record_position, recorded) = exact(&proof.recorded_event_id, "TaskRecorded")?;
    let (merge_position, merged) = exact(&proof.merge_executed_event_id, "MergeExecuted")?;
    if record_position <= merge_position || recorded.actor != "runtime:orch"
        || recorded.payload.as_ref() != Some(&json!({"postMergeGates":"all-green"}))
        || merged.payload.as_ref().and_then(|p| p["mergeSha"].as_str()) != Some(&authorization.merge_sha) {
        bail!("legacy debug Recorded/merge proof mismatch");
    }
    let local: Vec<_> = owner.events.iter().filter(|e| e.kind == "DispatchIssued" && e.actor == "runtime:orch"
        && e.task_id.as_deref() == Some(&proof.task_id) && e.round.as_deref() == Some(&entry.round)
        && e.payload.as_ref().is_some_and(|p| p["attemptId"] == authorization.attempt_id
            && p["agent"] == "local" && p["method"] == "local-worktree" && p["wakePending"] == false)).collect();
    if local.len() != 1 || owner.events[record_position + 1..].iter().any(|e|
        e.task_id.as_deref() == Some(&proof.task_id) && matches!(e.kind.as_str(),
            "DispatchIssued" | "WorkspaceLeased" | "ReportCollectClaimed" | "ReportCollectExecuting" | "ReviewRequested" | "WakeIssued")) {
        bail!("legacy debug task has no unique local dispatch or has later activity");
    }
    for (ancestor, descendant) in [(&bound.source.head, &authorization.head_sha),
        (&authorization.head_sha, &authorization.merge_sha), (&authorization.merge_sha, &main)] {
        git(root, &["merge-base", "--is-ancestor", ancestor, descendant])
            .context("legacy debug requires actual Git descendant chain")?;
    }
    if git(root, &["rev-parse", "refs/heads/main"])? != main { bail!("legacy debug main changed during authorization"); }
    Ok(proof.task_id.clone())
}

fn retirable(p: &str) -> bool {
    let path = Path::new(p);
    if !path.components().all(|p| matches!(p, Component::Normal(_))) { return false; }
    let count = path.components().count();
    ((p.starts_with(".cowork-temp/") || p.starts_with(".worktrees/"))
        && (count == 2 || (count == 4 && p.ends_with("/orch/target"))
            || (count == 5 && (p.ends_with("/orch/target/debug") || p.ends_with("/orch/target/test-tmp")))))
        || (p.starts_with("coordination/runtime/diagnostic-cache/") && count == 5 && p.ends_with("/target"))
        || p == "orch/target/test-tmp" || p == "orch/target/release"
}
fn git(path: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(["-c","core.fsmonitor=false","-c","core.hooksPath=/dev/null",
        "-c","gc.auto=0","-c","maintenance.auto=false","--no-optional-locks","-C"])
        .arg(path).args(args).env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE").env_remove("GIT_COMMON_DIR").env_remove("GIT_NAMESPACE")
        .env_remove("GIT_OBJECT_DIRECTORY").env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_REPLACE_REF_BASE").env("GIT_NO_REPLACE_OBJECTS", "1").output()?;
    if !out.status.success() { bail!("retirement Git refused: {}", String::from_utf8_lossy(&out.stderr)); }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
fn quiet(path: &Path) -> Result<()> {
    let lsof = ["/usr/sbin/lsof","/usr/bin/lsof"].into_iter().find(|p| Path::new(p).is_file())
        .context("lsof unavailable; current use unknown")?;
    let out = Command::new(lsof).args(["-nP","-t","+D"]).arg(path).output()?;
    if out.status.code() != Some(1) || !out.stdout.is_empty() || !out.stderr.is_empty() {
        bail!("retirement current use not proven quiet: {}", path.display());
    }
    // A sleeping runner may have no current file descriptor in the tree.
    let out = Command::new("/bin/ps").args(["-axo","pid=,command="]).output()?;
    if !out.status.success() || !out.stderr.is_empty() { bail!("retirement process observation unavailable"); }
    let needle = path.to_str().context("process subject not UTF-8")?;
    if String::from_utf8(out.stdout)?.lines().any(|line| line.contains(needle)) {
        bail!("retirement process command still references exact scene");
    }
    Ok(())
}
fn pid_absent(pid: u64) -> Result<()> {
    let pid = u32::try_from(pid).context("invalid recorded producer PID")?;
    if pid == 0 || crate::wake::managed_process_topology_snapshot_contains(pid)? {
        bail!("retirement producer/runner may still be present");
    }
    Ok(())
}
fn source_root(root: &Path, entry: &Entry, physical: &Path) -> Result<PathBuf> {
    if let Some(p) = &entry.preservation_root {
        let source = checked(root, &p.path)?;
        if !retirable(&p.path) || Path::new(&p.path).components().count() != 2
            || entry.excluded_caches.len() != 1
            || entry.path != format!("{}/{}", p.path, entry.excluded_caches[0])
            || entry.kind != "directory"
            || identity(&source)? != (p.device, p.inode) {
            bail!("cache retirement requires exact preserved parent and fixed Cargo target");
        }
        Ok(source)
    } else { Ok(physical.to_path_buf()) }
}
fn owner_guard(root: &Path, path: &Path, entry: &Entry) -> Result<crate::reclaim::MaintenanceInventory> {
    let inv = crate::reclaim::storage_inventory(root);
    if let Some(e) = &inv.global_hold { bail!("retirement ownership inventory incomplete: {e}"); }
    if !inv.errors.is_empty() { bail!("retirement ownership read failed"); }
    let owner = inv.rounds.iter().find(|r| r.round == entry.round).context("retirement owner round absent")?;
    let source = source_root(root, entry, path)?;
    let mut tasks = BTreeSet::new();
    for r in &inv.rounds {
        let overlapping = inv.claims.iter().any(|(id,p)| id.starts_with(&format!("{}/",r.round)) && overlaps(&source,p));
        if overlapping && (r.error.is_some() || r.round != entry.round) {
            bail!("retirement conflicting/ambiguous round ownership: {}",r.round);
        }
        if overlapping {
            for site in &r.sites {
                if overlaps(&source,&root.join(&site.worktree)) || overlaps(&source,&root.join(&site.target)) {
                    tasks.insert(site.task_id.clone());
                    if site.wake_id.is_some() {
                        crate::sites::maintenance_evidence(root,&r.round,&r.events,site)
                            .context("retirement native quiescence/artifact evidence unproved; historical unknown stays unknown")?;
                    }
                    let recorded_local = site.wake_id.is_none() && r.events.iter().any(|e|
                        e.kind == "TaskRecorded" && e.actor == "runtime:orch" && e.task_id.as_deref() == Some(&site.task_id));
                    if !closed(&r.events,&r.round) && !recorded_local {
                        bail!("retirement current round task/lease is not ended");
                    }
                }
            }
        }
        // An undecodable open round cannot be treated as an absent current owner.
        if !closed(&r.events,&r.round) && r.error.is_some() { bail!("retirement current ownership unknown"); }
    }
    if owner.error.is_some() { bail!("retirement owner ledger/WAL ambiguity"); }
    if entry.legacy_debug_retirement.is_some() {
        tasks.insert(legacy_debug_authority(root, entry, &inv)?);
    }
    if !closed(&owner.events,&entry.round) && tasks.is_empty() {
        bail!("retirement owner is neither closed nor a Recorded local task");
    }
    for p in receipt_paths(root)? {
        let v = read_receipt(root,&p)?;
        let subject = receipt_subject(root,&v)?;
        if overlaps(&source,&subject) {
            if v.pointer("/owner/round").and_then(Value::as_str) != Some(entry.round.as_str()) {
                bail!("retirement manifest contradicts snapshot owner");
            }
            if let Some(task) = v.pointer("/owner/taskId").and_then(Value::as_str) { tasks.insert(task.to_string()); }
            if let Some(pid) = v["producerPid"].as_u64() { pid_absent(pid)?; }
            if present(&subject)? && v["device"].as_u64().zip(v["inode"].as_u64()) != Some(identity(&subject)?) {
                bail!("retirement snapshot identity changed or unproven");
            }
        }
    }
    for p in entries(&checked(root,"coordination/runtime/heartbeats")?)? {
        if p.extension().is_none_or(|e| e != "json") { continue; }
        let v: Value = serde_json::from_slice(&regular_bytes(&p,1024*1024)?)?;
        if v["generation"].as_str().is_some_and(|g| tasks.iter().any(|t| g.starts_with(&format!("{t}-")))) {
            pid_absent(v["pid"].as_u64().context("retirement runner PID unknown")?)?;
        }
    }
    if let Some((policy,_)) = load(root)? {
        let current = round_number(&crate::current_round(root)?)?;
        let first = current.saturating_sub(policy.keep_recent_rounds-1).max(1);
        for (subject, owner) in custody_sources(root, &policy, current, &inv, Some((&source, entry)))? {
            if overlaps(&source, &subject) && owner != entry.round {
                bail!("retirement manifest contradicts declared round custody");
            }
        }
        if entry.preservation_root.is_none() && round_number(&entry.round)? >= first {
            bail!("retirement subject belongs to retained round window");
        }
    }
    Ok(inv)
}
fn archive_path(root: &Path, relative: &str, source: &Path) -> Result<PathBuf> {
    let path = checked(root,relative)?;
    if !Path::new(relative).starts_with(ARCHIVES) || Path::new(relative) == Path::new(ARCHIVES)
        || overlaps(source,&path) { bail!("retirement backup outside project archive or overlapping source"); }
    Ok(path)
}
fn backup_guard(root: &Path, entry: &Entry) -> Result<PathBuf> {
    let source = root.join(entry.preservation_root.as_ref().map_or(entry.path.as_str(), |p|p.path.as_str()));
    let backup = archive_path(root,&entry.backup_path,&source)?;
    if preservation_digest(&backup,&entry.excluded_caches)? != entry.source_digest {
        bail!("retirement backup digest mismatch; recovery requires the preserved original");
    }
    if let Some(g) = &entry.git_preservation {
        for (relative,digest) in [(&g.common_backup,&g.common_digest),(&g.private_backup,&g.private_digest)] {
            let b = archive_path(root,relative,&source)?;
            if preservation_digest(&b,&[])? != *digest { bail!("retirement Git backup digest mismatch"); }
        }
    }
    Ok(backup)
}
fn reject_git_in_excluded_cache(path: &Path) -> Result<()> {
    let metadata=fs::symlink_metadata(path)?;
    if path.file_name().is_some_and(|n|n==".git") {
        bail!("retirement excluded cache contains Git state; preserve and native-retire it separately");
    }
    if metadata.is_dir() {
        for child in entries(path)? { reject_git_in_excluded_cache(&child)?; }
    }
    Ok(())
}
fn reject_nested_git(base: &Path, path: &Path, excluded: &[String]) -> Result<()> {
    if excluded.iter().any(|p| path.strip_prefix(base).is_ok_and(|r|r.starts_with(p))) { return Ok(()); }
    let m=fs::symlink_metadata(path)?;
    if path != base && path.file_name().is_some_and(|n|n==".git") {
        if path.parent()==Some(base) { return Ok(()); }
        bail!("retirement nested Git boundary needs independent preservation and native removal");
    }
    if m.is_dir() { for child in entries(path)? { reject_nested_git(base,&child,excluded)?; } }
    Ok(())
}
fn validate_preserved_source(root: &Path, entry: &Entry, physical: &Path) -> Result<()> {
    cache_exclusions(&entry.excluded_caches)?;
    let source = source_root(root, entry, physical)?;
    let backup = backup_guard(root, entry)?;
    if entry.legacy_debug_retirement.is_some() {
        owner_guard(root, physical, entry)?;
    }
    if preservation_digest(&source,&entry.excluded_caches)? != entry.source_digest {
        bail!("retirement source digest changed; interrupted deletion HOLD: restore from backup, never infer completion");
    }
    independent_copy(&source,&backup,&entry.excluded_caches)?;
    if let Some(head) = &entry.head {
        if head.len()!=40 || !head.bytes().all(|b| b.is_ascii_hexdigit())
            || git(&source,&["rev-parse","HEAD"])? != *head
            || fs::canonicalize(git(&source,&["rev-parse","--show-toplevel"])?)? != fs::canonicalize(&source)? {
            bail!("retirement Git HEAD/root changed");
        }
        let tracked = git(&source,&["ls-files","-z"])?;
        if tracked.split('\0').any(|p| entry.excluded_caches.iter().any(|c|Path::new(p).starts_with(c))) {
            bail!("retirement exclusion contains tracked source");
        }
        let common = PathBuf::from(git(&source,&["rev-parse","--path-format=absolute","--git-common-dir"])?);
        if present(&common.join("objects/info/alternates"))? { bail!("retirement Git borrows external objects"); }
        let private = PathBuf::from(git(&source,&["rev-parse","--path-format=absolute","--git-dir"])?);
        let linked = private != common;
        if (entry.kind=="linked-worktree") != (linked && entry.preservation_root.is_none()) {
            bail!("retirement Git layout/kind mismatch");
        }
        let restore = if linked {
            let g = entry.git_preservation.as_ref().context("linked retirement needs full common/private Git and restore proof")?;
            if fs::canonicalize(&common)? != fs::canonicalize(root.join(".git"))? {
                bail!("linked retirement belongs to another project");
            }
            for (original,relative,digest) in [(&common,&g.common_backup,&g.common_digest),(&private,&g.private_backup,&g.private_digest)] {
                let b = archive_path(root,relative,&source)?;
                if preservation_digest(original,&[])? != *digest { bail!("retirement live Git state changed"); }
                independent_copy(original,&b,&[])?;
            }
            let probe = archive_path(root,&g.restored_worktree,&source)?;
            let mut exclusions = entry.excluded_caches.clone(); exclusions.push(".git".into());
            let tree_digest = |p:&Path| -> Result<Vec<u8>> {let mut h=Sha256::new();digest_walk(p,p,&exclusions,&mut h)?;Ok(h.finalize().to_vec())};
            if tree_digest(&source)? != tree_digest(&probe)? { bail!("retirement restored worktree content differs"); }
            if present(&private.join("index"))? && fs::read(private.join("index"))? != fs::read(probe.join(".git/index"))? {
                bail!("retirement restored index differs");
            }
            probe
        } else {
            if fs::canonicalize(&common)? != fs::canonicalize(source.join(".git"))? {
                bail!("independent retirement borrows Git storage");
            }
            backup.clone()
        };
        if fs::canonicalize(git(&restore,&["rev-parse","--show-toplevel"])?)? != fs::canonicalize(&restore)?
            || fs::canonicalize(git(&restore,&["rev-parse","--path-format=absolute","--git-common-dir"])?)? != fs::canonicalize(restore.join(".git"))?
            || git(&restore,&["rev-parse","HEAD"])? != *head
            || git(&restore,&["status","--porcelain=v1","--untracked-files=all"])? != git(&source,&["status","--porcelain=v1","--untracked-files=all"])? {
            bail!("retirement restored Git state differs");
        }
        git(&restore,&["fsck","--full","--no-dangling"])?;
    } else if entry.kind != "directory" || present(&source.join(".git"))? || !entry.excluded_caches.is_empty() {
        bail!("retirement Git identity omitted or non-Git cache exclusion");
    }
    quiet(&source)?;
    Ok(())
}
fn validate_entry(root: &Path, entry: &Entry, physical: &Path) -> Result<()> {
    if !retirable(&entry.path) || identity(physical)? != (entry.device,entry.inode) {
        bail!("retirement protected path or changed identity");
    }
    cache_exclusions(&entry.excluded_caches)?;
    let source = source_root(root,entry,physical)?;
    if entry.preservation_root.is_none() && entry.archive_path.is_none() {
        reject_nested_git(&source,&source,&entry.excluded_caches)?;
    }
    // Excluding bytes from a backup is not proof that an independent repository
    // accidentally placed in a Cargo target is disposable.
    for excluded in &entry.excluded_caches {
        let cache=if entry.preservation_root.is_some() {physical.to_path_buf()} else {source.join(excluded)};
        if present(&cache)? { reject_git_in_excluded_cache(&cache)?; }
    }

    validate_preserved_source(root, entry, physical)?;
    if physical != source { quiet(physical)?; }
    Ok(())
}

fn ancestors(root: &Path, path: &Path) -> Result<Vec<(PathBuf,(u64,u64))>> {
    let mut out = vec![(root.to_path_buf(),identity(root)?)];
    let mut p = root.to_path_buf();
    for component in path.strip_prefix(root)?.components() {
        p.push(component); out.push((p.clone(),identity(&p)?));
    }
    Ok(out)
}
fn same_ancestors(root: &Path, before: &[(PathBuf,(u64,u64))]) -> Result<()> {
    for (p,id) in before {
        if p != root { checked(root,p.strip_prefix(root)?.to_str().context("ancestor path UTF-8")?)?; }
        if identity(p)? != *id { bail!("retirement ancestor replaced"); }
    }
    Ok(())
}
fn rename_new(source: &Path, target: &Path) -> Result<()> {
    use std::ffi::CString;
    let a=CString::new(source.as_os_str().as_encoded_bytes())?;
    let b=CString::new(target.as_os_str().as_encoded_bytes())?;
    #[cfg(target_os="macos")]
    let rc = {
        unsafe extern "C" { fn renamex_np(a:*const std::os::raw::c_char,b:*const std::os::raw::c_char,flags:u32)->i32; }
        // RENAME_EXCL prevents even an empty destination from being overwritten.
        unsafe { renamex_np(a.as_ptr(),b.as_ptr(),4) }
    };
    #[cfg(target_os="linux")]
    let rc = {
        unsafe extern "C" { fn renameat2(ad:i32,a:*const std::os::raw::c_char,bd:i32,b:*const std::os::raw::c_char,flags:u32)->i32; }
        unsafe { renameat2(-100,a.as_ptr(),-100,b.as_ptr(),1) }
    };
    #[cfg(not(any(target_os="macos",target_os="linux")))]
    let rc = { let _=(a,b); bail!("exclusive retirement rename unsupported"); };
    if rc != 0 { return Err(std::io::Error::last_os_error().into()); }
    Ok(())
}
/// Validate/apply one exact, explicitly authorized retirement; dry-run creates no locks or files.
/// A full source/Git backup and restore proof precede effects. Only reproducible target data may
/// be omitted. Old ledger/native facts remain unchanged. Current use or ownership uncertainty holds.
/// Optional intact directory archival keeps raw fixture topology and reports zero deletion.
/// Completed receipts replay without deletion; replacements fail. Interrupted partial trees hold
/// with the backup as the recovery source; they are never treated as successful deletion.
/// A separate legacyDebugRetirement proof permits only preserved debug after canonical local Recorded
/// authorization and real Git ancestry, with kept-source/proof checks even on absent-cache replay.
pub fn retire_manifest(root: &Path, manifest: &Path, dry_run: bool) -> Result<Value> {
    crate::reclaim::validate_storage_root(root)?;
    let bytes=regular_bytes(manifest,1024*1024)?;
    let m:Manifest=serde_json::from_slice(&bytes)?;
    if m.schema_version!=1 || m.project_root!=root || m.authorization.trim().is_empty() || m.entries.len()!=1 {
        bail!("retirement requires one exact authorized object");
    }
    if SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()>m.expires_at { bail!("retirement authorization expired"); }
    let id=hex::encode(Sha256::digest(&bytes));
    let entry=&m.entries[0];
    legacy_debug_shape(entry)?;
    if !retirable(&entry.path) { bail!("retirement protected path"); }
    if entry.archive_path.is_some() { return archive_scene(root,entry,&id,dry_run); }
    if entry.path.ends_with("/orch/target/test-tmp") { bail!("raw fixture trees require intact archive mode"); }
    let path=checked(root,&entry.path)?;
    let parent=path.parent().context("retirement parent")?;
    let mut before=ancestors(root,parent)?;
    // Keep a cache quarantine outside its preserved source root, so the source
    // digest still describes exactly the same non-cache tree after the move.
    let quarantine_parent=if let Some(p)=&entry.preservation_root {
        checked(root,&p.path)?.parent().context("cache source parent")?.to_path_buf()
    } else {parent.to_path_buf()};
    before.extend(ancestors(root,&quarantine_parent)?);
    let quarantine=quarantine_parent.join(format!(".retiring-{id}"));
    let receipt=checked(root,&format!("{RETIREMENTS}/{id}.json"))?;
    let proof=|phase:&str| json!({"schemaVersion":1,"manifestSha256":id,"entry":entry,
        "device":entry.device,"inode":entry.inode,"phase":phase,"quarantine":quarantine,
        "observationBoundary":"explicit local scene retirement; no assertion of historical native completion",
        "recovery":"restore preserved source and Git data; rebuild excluded Cargo target; never rewrite old ledger"});
    let action=||->Result<Value> {
        same_ancestors(root,&before)?;
        let pinned_main = if entry.legacy_debug_retirement.is_some() { Some(git(root, &["rev-parse", "refs/heads/main"])?) } else { None };
        let check_main = || -> Result<()> {
            if let Some(expected) = &pinned_main {
                if git(root, &["rev-parse", "refs/heads/main"])? != *expected { bail!("legacy debug main changed before effect"); }
            }
            Ok(())
        };
        let validate_physical = |physical: &Path| -> Result<()> {
            check_main()?; validate_entry(root, entry, physical)?; check_main()
        };
        let validate_absent = || -> Result<()> {
            if entry.legacy_debug_retirement.is_some() {
                check_main()?; validate_preserved_source(root, entry, &path)?; check_main()?;
            } else { backup_guard(root, entry)?; }
            Ok(())
        };
        let old:Option<Value>=if present(&receipt)? {Some(serde_json::from_slice(&regular_bytes(&receipt,1024*1024)?)?)} else {None};
        if let Some(v)=&old {
            if v["manifestSha256"]!=id || v["entry"]!=serde_json::to_value(entry)?
                || v["device"]!=entry.device || v["inode"]!=entry.inode || v["quarantine"]!=json!(quarantine) {
                bail!("retirement receipt identity mismatch");
            }
            if v["phase"]=="removed" {
                if present(&path)? || present(&quarantine)? { bail!("retirement ABA after completed receipt"); }
                validate_absent()?;
                return Ok(json!({"dryRun":dry_run,"items":[{"path":entry.path,"disposition":"already-removed","newlyRemovedBytes":0}]}));
            }
            if v["phase"]!="prepared" && v["phase"]!="renamed" { bail!("retirement unknown receipt phase"); }
        }
        let inv=owner_guard(root,&path,entry)?;
        let path_present=present(&path)?;
        let moved=present(&quarantine)?;
        if !path_present && !moved {
            if old.is_none() { bail!("retirement absent subject without prior effect receipt"); }
            validate_absent()?;
            if entry.kind=="linked-worktree" && git(root,&["worktree","list","--porcelain"])?
                .lines().any(|line|line.strip_prefix("worktree ")==path.to_str()) {
                bail!("interrupted linked retirement registry remains; preserve backup and use native Git recovery");
            }
            if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("retirement ownership changed"); }
            if !dry_run { atomic_json(root,&receipt,&proof("removed"))?; }
            return Ok(json!({"dryRun":dry_run,"items":[{"path":entry.path,"disposition":"reconciled-absent","newlyRemovedBytes":0}]}));
        }
        let physical=if moved {
            if old.is_none() || path_present || entry.kind=="linked-worktree" { bail!("retirement quarantine recovery ambiguous"); }
            &quarantine
        } else { &path };
        checked(root,physical.strip_prefix(root)?.to_str().context("retirement path UTF-8")?)?;
        validate_physical(physical)?;
        if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("retirement ownership changed"); }
        if dry_run { return Ok(json!({"dryRun":true,"items":[{"path":entry.path,"disposition":"eligible","manifestSha256":id}]})); }
        if entry.kind=="linked-worktree" {
            atomic_json(root,&receipt,&proof("prepared"))?;
            same_ancestors(root,&before)?;
            validate_physical(&path)?;
            if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("retirement ownership changed"); }
            git(root,&["worktree","remove","--force",path.to_str().context("worktree path UTF-8")?])?;
        } else {
            if !moved {
                atomic_json(root,&receipt,&proof("prepared"))?;
                same_ancestors(root,&before)?;
                validate_physical(&path)?;
                if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("retirement ownership changed"); }
                rename_new(&path,&quarantine)?;
                fs::File::open(parent)?.sync_all()?;
                fs::File::open(&quarantine_parent)?.sync_all()?;
                atomic_json(root,&receipt,&proof("renamed"))?;
            }
            same_ancestors(root,&before)?;
            validate_physical(&quarantine)?;
            if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("retirement ownership changed after rename"); }
            crate::util::remove_dir_all_with_enotempty_retry(&quarantine)?;
        }
        if present(&path)? || present(&quarantine)? { bail!("retirement physical removal incomplete; preserved backup required"); }
        atomic_json(root,&receipt,&proof("removed"))?;
        Ok(json!({"dryRun":false,"items":[{"path":entry.path,"disposition":"removed","receipt":receipt}],"physicalFreeSpaceClaim":false}))
    };
    if dry_run { action() } else { crate::reclaim::with_maintenance_ledger(root,action) }
}

/// The optional assessment used by the existing maintenance report.
pub(crate) fn maintenance_assessment(root: &Path) -> Result<Option<Value>> {
    if load(root)?.is_none() { return Ok(None); }
    assess(root, &crate::current_round(root)?)
}

fn archive_scene(root: &Path, entry: &Entry, id: &str, dry_run: bool) -> Result<Value> {
    if entry.kind!="directory" || entry.preservation_root.is_some() || !entry.excluded_caches.is_empty() {
        bail!("intact archive requires one fully preserved directory without cache exclusions");
    }
    let source=checked(root,&entry.path)?;
    let archive=archive_path(root,entry.archive_path.as_deref().context("archive path missing")?,&source)?;
    let backup=backup_guard(root,entry)?;
    if overlaps(&archive,&backup) { bail!("archive destination overlaps its preservation backup"); }
    let source_parent=source.parent().context("archive source parent")?;
    let archive_parent=archive.parent().context("archive destination parent")?;
    let mut before=ancestors(root,source_parent)?;
    before.extend(ancestors(root,archive_parent)?);
    let receipt=checked(root,&format!("{RETIREMENTS}/{id}.json"))?;
    let proof=|phase:&str|json!({"schemaVersion":1,"manifestSha256":id,"entry":entry,
        "device":entry.device,"inode":entry.inode,"phase":phase,"archivePath":archive,
        "newlyRemovedBytes":0,"recovery":"verify digest, ensure original path absent, move intact archive back to original absolute path; old fixture pointers and failures stay unchanged"});
    let verify_archive=||->Result<()> {
        same_ancestors(root,&before)?;
        if present(&source)? || identity(&archive)?!=(entry.device,entry.inode)
            || preservation_digest(&archive,&[])?!=entry.source_digest {
            bail!("archive source reappeared, identity changed or bytes drifted");
        }
        backup_guard(root,entry)?;
        Ok(())
    };
    let action=||->Result<Value> {
        same_ancestors(root,&before)?;
        let old:Option<Value>=if present(&receipt)? {Some(serde_json::from_slice(&regular_bytes(&receipt,1024*1024)?)?)} else {None};
        if let Some(v)=&old {
            if v["manifestSha256"]!=id || v["entry"]!=serde_json::to_value(entry)?
                || v["archivePath"]!=json!(archive) { bail!("archive receipt identity mismatch"); }
            if v["phase"]=="archived" {
                verify_archive()?;
                return Ok(json!({"dryRun":dry_run,"items":[{"path":entry.path,"disposition":"already-archived","archivePath":archive,"newlyRemovedBytes":0}]}));
            }
            if v["phase"]!="archive-prepared" { bail!("archive receipt phase invalid"); }
        }
        let inv=owner_guard(root,&source,entry)?;
        if present(&archive)? {
            if old.is_none() { bail!("archive destination already exists without prior intent"); }
            verify_archive()?;
            if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("archive ownership changed"); }
            if !dry_run { atomic_json(root,&receipt,&proof("archived"))?; }
            return Ok(json!({"dryRun":dry_run,"items":[{"path":entry.path,"disposition":"reconciled-archive","archivePath":archive,"newlyRemovedBytes":0}]}));
        }
        validate_entry(root,entry,&source)?;
        if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("archive ownership changed"); }
        if dry_run { return Ok(json!({"dryRun":true,"items":[{"path":entry.path,"disposition":"eligible-archive","archivePath":archive,"newlyRemovedBytes":0}]})); }
        atomic_json(root,&receipt,&proof("archive-prepared"))?;
        same_ancestors(root,&before)?;
        validate_entry(root,entry,&source)?;
        if !crate::reclaim::maintenance_inventory_unchanged(root,&inv) { bail!("archive ownership changed"); }
        rename_new(&source,&archive)?;
        fs::File::open(source_parent)?.sync_all()?;
        fs::File::open(archive_parent)?.sync_all()?;
        verify_archive()?;
        atomic_json(root,&receipt,&proof("archived"))?;
        Ok(json!({"dryRun":false,"items":[{"path":entry.path,"disposition":"archived","archivePath":archive,"newlyRemovedBytes":0}],"physicalFreeSpaceClaim":false}))
    };
    if dry_run { action() } else { crate::reclaim::with_maintenance_ledger(root,action) }
}

#[cfg(test)]
mod legacy_debug_history_tests {
    use super::*;
    #[test]
    fn replacement_graft_and_shallow_history_are_not_authority() {
        let root = crate::util::test_scratch_dir("legacy-debug-history");
        git(&root, &["init", "-q", "-b", "main"]).unwrap();
        fs::write(root.join("data"), "original").unwrap();
        git(&root, &["add", "."]).unwrap();
        git(&root, &["-c", "user.name=Fixture", "-c", "user.email=f@example.invalid", "commit", "-qm", "fixture"]).unwrap();
        ordinary_git_history(&root).unwrap();
        let head = git(&root, &["rev-parse", "HEAD"]).unwrap();
        let replacement = root.join(".git/refs/replace").join(&head);
        fs::create_dir_all(replacement.parent().unwrap()).unwrap();
        fs::write(&replacement, format!("{head}\n")).unwrap();
        assert!(ordinary_git_history(&root).is_err());
        fs::remove_file(replacement).unwrap();
        let graft = root.join(".git/info/grafts"); fs::write(&graft, "").unwrap();
        assert!(ordinary_git_history(&root).is_err()); fs::remove_file(graft).unwrap();
        let shallow = root.join(".git/shallow"); fs::write(&shallow, format!("{head}\n")).unwrap();
        assert!(ordinary_git_history(&root).is_err()); fs::remove_file(shallow).unwrap();
        ordinary_git_history(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
