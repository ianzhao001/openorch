//! Trial build-cache identity and bounded slot management.
//!
//! A trial slot has a stable source path and a private target directory.  The target marker is
//! deliberately stronger than Cargo's own mtime-based fingerprints: it binds the repository,
//! source path, target path, toolchain, lockfile and build command summary.  A mismatching or
//! unreadable generation is never repaired in place; it remains as evidence and a new empty
//! generation is selected before any gate log is created or child is spawned.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use orch_core::EventRecord;

use crate::binding::CommandSpec;

pub const MARKER_FILE: &str = ".orch-build-identity.json";
pub const TRIAL_SLOT_COUNT: usize = 4;
/// One complete gate build is budgeted at 38 GiB by the storage admission layer.  A single
/// retained trial slot larger than that cannot be justified as a warm-start cache: it consumes
/// more space than the operation whose rebuild it is meant to avoid.
pub const DEFAULT_TRIAL_SLOT_BUDGET_BYTES: u64 = 38 * 1024 * 1024 * 1024;
const GENERATION_PREFIX: &str = "generation-";
const BUILD_IDENTITY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrialCacheSweepReport {
    pub removed: usize,
    pub freed_bytes: u64,
    /// Logical bytes observed under every slot immediately before and after a budgeted sweep.
    /// This is deterministic cache visibility, not a claim about APFS physical blocks released.
    pub slot_usage: Vec<TrialCacheSlotUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialCacheSlotUsage {
    pub slot: String,
    pub logical_bytes_before: u64,
    pub logical_bytes_after: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SweepTargetsReport {
    pub removed: Vec<String>,
    pub refused: Vec<String>,
    pub preserved: Vec<String>,
    /// Preserved paths whose current-round lease is still active.
    pub(crate) active_preserved: Vec<String>,
    /// Worktree paths governed by the same active leases.
    pub(crate) active_worktrees: Vec<String>,
    /// Per-path deletion failures retained after the sweep enters its mutation phase.
    pub failures: Vec<String>,
    /// A removal error may have partially changed a target before returning.
    pub incomplete: bool,
    pub freed_bytes: u64,
    pub debug_bytes: u64,
    /// Per-path accounting retained for `survey_disk`; the public aggregate remains authoritative.
    pub(crate) removed_bytes: BTreeMap<String, u64>,
    /// TTL- or Active-protected sweep targets covered by a concrete future cleanup mechanism.
    pub(crate) governed_preserved: BTreeSet<String>,
}

fn payload_string<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
}

fn payload_u64(event: &EventRecord, key: &str) -> Option<u64> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_u64)
}

fn lease_target(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("paths"))
        .and_then(|paths| paths.get("target"))
        .and_then(serde_json::Value::as_str)
}

fn safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn target_path_is_direct_child(target: &str) -> bool {
    let mut components = Path::new(target).components();
    matches!(components.next(), Some(std::path::Component::Normal(value)) if value == "orch")
        && matches!(components.next(), Some(std::path::Component::Normal(value)) if value == "target")
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn target_path_is_under_target_root(target: &str) -> bool {
    let mut components = Path::new(target).components();
    matches!(components.next(), Some(std::path::Component::Normal(value)) if value == "orch")
        && matches!(components.next(), Some(std::path::Component::Normal(value)) if value == "target")
}

#[derive(Debug, Clone)]
struct TargetLease {
    index: usize,
    task_id: String,
    generation: u64,
    attempt_id: String,
    role: String,
    agent: String,
    wake_id: Option<String>,
    worktree: String,
    target: Option<String>,
}

fn parse_target_lease(index: usize, event: &EventRecord) -> Result<(String, TargetLease)> {
    if event.actor != "runtime:orch" {
        bail!("WorkspaceLeased actor is not canonical");
    }
    let site_id = payload_string(event, "siteId")
        .filter(|value| safe_component(value))
        .context("WorkspaceLeased missing siteId")?;
    let task_id = event
        .task_id
        .as_deref()
        .filter(|value| safe_component(value))
        .context("WorkspaceLeased missing taskId")?;
    let generation = payload_u64(event, "generation")
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .context("WorkspaceLeased missing generation")?;
    let attempt_id = payload_string(event, "attemptId")
        .filter(|value| safe_component(value))
        .context("WorkspaceLeased missing attemptId")?;
    let role_value = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("role"))
        .context("WorkspaceLeased missing role")?;
    let role_text = role_value
        .as_str()
        .context("WorkspaceLeased invalid role type")?;
    let role = crate::sites::SiteRole::parse(role_text)
        .map_err(|error| anyhow::anyhow!("WorkspaceLeased invalid role: {error}"))?;
    // Parsing is shared; admitting a future site kind to destructive cache handling
    // still requires an explicit decision. A new enum variant must fail to compile here.
    let role = match role {
        crate::sites::SiteRole::Primary
        | crate::sites::SiteRole::Secondary
        | crate::sites::SiteRole::Nongate
        | crate::sites::SiteRole::Review
        | crate::sites::SiteRole::Implement => role.as_str(),
    };
    let agent = payload_string(event, "agent")
        .filter(|value| safe_component(value))
        .context("WorkspaceLeased missing agent")?;
    payload_string(event, "reviewedHead")
        .filter(|value| full_sha(value))
        .context("WorkspaceLeased reviewedHead is not a full SHA")?;
    let expected_site_id = format!("{task_id}-{role}-{agent}-g{generation:02}");
    if site_id != expected_site_id {
        bail!("WorkspaceLeased siteId does not match its identity");
    }
    let worktree = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("paths"))
        .and_then(|paths| paths.get("worktree"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| safe_relative_path(value))
        .context("WorkspaceLeased paths.worktree is not a safe relative path")?;
    let target = lease_target(event)
        .filter(|value| safe_relative_path(value))
        .context("WorkspaceLeased paths.target is not a safe relative path")?;
    let target = if target_path_is_direct_child(target) {
        Some(target.to_string())
    } else if target_path_is_under_target_root(target) {
        bail!("WorkspaceLeased target inside orch/target is not a direct child");
    } else {
        None
    };
    Ok((
        site_id.to_string(),
        TargetLease {
            index,
            task_id: task_id.to_string(),
            generation: u64::from(generation),
            attempt_id: attempt_id.to_string(),
            role: role.to_string(),
            agent: agent.to_string(),
            wake_id: payload_string(event, "wakeId")
                .filter(|value| safe_component(value))
                .map(str::to_string),
            worktree: worktree.to_string(),
            target,
        },
    ))
}

fn terminal_identity_matches(event: &EventRecord, lease: &TargetLease) -> bool {
    event.actor == "runtime:orch"
        && event.task_id.as_deref() == Some(lease.task_id.as_str())
        && payload_u64(event, "generation") == Some(lease.generation)
        && payload_string(event, "attemptId") == Some(lease.attempt_id.as_str())
        && payload_string(event, "role") == Some(lease.role.as_str())
        && payload_string(event, "agent") == Some(lease.agent.as_str())
        && payload_string(event, "wakeId") == lease.wake_id.as_deref()
}

fn retirement_anchor_matches(
    events: &[EventRecord],
    retirement: &EventRecord,
    lease: &TargetLease,
) -> bool {
    let Some(anchor_id) = payload_string(retirement, "retireEventId") else {
        return false;
    };
    let mut anchors = events.iter().filter(|event| event.event_id == anchor_id);
    let Some(anchor) = anchors.next() else {
        return false;
    };
    if anchors.next().is_some()
        || anchor.actor != "runtime:orch"
        || anchor.round != retirement.round
    {
        return false;
    }
    match anchor.kind.as_str() {
        "TaskRecorded" => anchor.task_id.as_deref() == Some(lease.task_id.as_str()),
        "RoundClosed" => anchor.task_id.is_none(),
        _ => false,
    }
}

fn terminal_event_matches(
    events: &[EventRecord],
    event: &EventRecord,
    lease: &TargetLease,
) -> bool {
    if !terminal_identity_matches(event, lease) {
        return false;
    }
    match event.kind.as_str() {
        "WorkspaceReleased" => {
            if payload_string(event, "completionReceipt")
                != Some("runtime:orch/managed-wake-terminated")
            {
                return false;
            }
            let Some(termination_id) = payload_string(event, "terminationEventId") else {
                return true;
            };
            let Some(wake_id) = lease.wake_id.as_deref() else {
                return false;
            };
            let terminations = events
                .iter()
                .filter(|candidate| {
                    candidate.kind == "ManagedWakeTerminated"
                        && candidate.actor == "runtime:orch"
                        && candidate.task_id.as_deref() == Some(lease.task_id.as_str())
                        && payload_string(candidate, "wakeId") == Some(wake_id)
                        && payload_string(candidate, "agent") == Some(lease.agent.as_str())
                        && candidate
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("managedScopeTerminated"))
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                })
                .collect::<Vec<_>>();
            terminations.len() == 1 && terminations[0].event_id == termination_id
        }
        "SiteRetired" => {
            payload_string(event, "taskId") == Some(lease.task_id.as_str())
                && matches!(
                    payload_string(event, "trigger"),
                    Some("task-recorded" | "round-close" | "manual")
                )
                && retirement_anchor_matches(events, event, lease)
        }
        _ => false,
    }
}

fn managed_termination_matches(event: &EventRecord, lease: &TargetLease) -> bool {
    let Some(wake_id) = lease.wake_id.as_deref() else {
        return false;
    };
    event.kind == "ManagedWakeTerminated"
        && event.actor == "runtime:orch"
        && event.task_id.as_deref() == Some(lease.task_id.as_str())
        && payload_string(event, "wakeId") == Some(wake_id)
        && payload_string(event, "agent") == Some(lease.agent.as_str())
        && event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("managedScopeTerminated"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

/// Fold active target paths without consulting worktree or process state. Any malformed lifecycle
/// event rejects the whole sweep so an ambiguous ledger can never widen the deletion set.
#[derive(Debug, Default)]
struct ActiveWorkspacePaths {
    targets: BTreeSet<String>,
    worktrees: BTreeSet<String>,
}

fn active_target_paths(events: &[EventRecord]) -> Result<ActiveWorkspacePaths> {
    let mut leases = BTreeMap::<String, TargetLease>::new();
    for (index, event) in events.iter().enumerate() {
        if event.kind == "WorkspaceLeased" {
            let (site_id, lease) = parse_target_lease(index, event)?;
            if leases.insert(site_id, lease).is_some() {
                bail!("WorkspaceLeased siteId is duplicated");
            }
        }
    }

    let mut terminal = BTreeSet::<String>::new();
    for (site_id, lease) in &leases {
        let releases = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "WorkspaceReleased"
                    && payload_string(event, "siteId") == Some(site_id.as_str())
            })
            .collect::<Vec<_>>();
        let retirements = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "SiteRetired"
                    && payload_string(event, "siteId") == Some(site_id.as_str())
            })
            .collect::<Vec<_>>();
        let terminations = events
            .iter()
            .enumerate()
            .filter(|(index, event)| {
                *index > lease.index && managed_termination_matches(event, lease)
            })
            .collect::<Vec<_>>();
        if releases.len() > 1 || retirements.len() > 1 {
            bail!("workspace terminal event is duplicated");
        }
        for (index, event) in releases.iter().chain(retirements.iter()).copied() {
            if index <= lease.index || !terminal_event_matches(events, event, lease) {
                bail!("workspace terminal event is out of order or does not match its lease");
            }
        }
        if !releases.is_empty() || !retirements.is_empty() || terminations.len() == 1 {
            terminal.insert(site_id.clone());
        }
    }

    for event in events
        .iter()
        .filter(|event| matches!(event.kind.as_str(), "WorkspaceReleased" | "SiteRetired"))
    {
        let site_id = payload_string(event, "siteId")
            .filter(|value| !value.is_empty())
            .context("workspace terminal event missing siteId")?;
        if !leases.contains_key(site_id) {
            bail!("workspace terminal event has no lease");
        }
    }

    let mut active = ActiveWorkspacePaths::default();
    for (site_id, lease) in leases {
        if terminal.contains(&site_id) {
            continue;
        }
        active.worktrees.insert(lease.worktree);
        if let Some(target) = lease.target {
            active.targets.insert(target);
        }
    }
    Ok(active)
}

fn target_relative_path(name: &str) -> String {
    format!("orch/target/{name}")
}

fn target_name_is_sweepable(name: &str) -> bool {
    name.starts_with("review-") || name.starts_with("consult-") || name.starts_with("t-")
}

fn old_enough(path: &Path, ttl: Duration) -> bool {
    if ttl.is_zero() {
        return true;
    }
    fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= ttl)
}

/// Sweep direct child directories on a conservative name whitelist.
/// Active lease targets (including schema-3 `review`), young entries, non-directories,
/// unknown names and entries with a top-level `.git` are kept. Role validation shares
/// [`crate::sites::SiteRole`] with the site lifecycle; missing, ill-typed and unknown
/// roles fail before any deletion. A future enum variant requires explicit cache policy.
pub fn sweep_targets(
    root: &Path,
    events: &[EventRecord],
    ttl: Duration,
) -> Result<SweepTargetsReport> {
    let eligible = terminal_target_paths(events)?;
    sweep_targets_preserving(root, events, ttl, &BTreeSet::new(), Some(&eligible))
}

fn sweep_targets_preserving(
    root: &Path,
    events: &[EventRecord],
    ttl: Duration,
    protected_targets: &BTreeSet<String>,
    eligible_targets: Option<&BTreeSet<String>>,
) -> Result<SweepTargetsReport> {
    use std::os::unix::fs::MetadataExt;
    let target_root = crate::reclaim::checked_storage_path(root, Path::new("orch/target"))?;
    let target_metadata = match fs::symlink_metadata(&target_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut report = SweepTargetsReport::default();
            if let Ok(active) = active_target_paths(events) {
                report.active_worktrees = active.worktrees.into_iter().collect();
            }
            return Ok(report);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat target root failed: {}", target_root.display()))
        }
    };
    if target_metadata.file_type().is_symlink() || !target_metadata.is_dir() {
        bail!(
            "target root is not a real directory: {}",
            target_root.display()
        );
    }
    let directory = match fs::read_dir(&target_root) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read target root failed: {}", target_root.display()))
        }
    };
    let mut entries = directory.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let active_paths = active_target_paths(events)?;

    let mut report = SweepTargetsReport::default();
    if let Some(debug) = entries
        .iter()
        .find(|entry| entry.file_name() == std::ffi::OsStr::new("debug"))
    {
        if debug.file_type()?.is_dir() {
            report.debug_bytes = apparent_tree_bytes(&debug.path())?;
        }
    }

    let mut removals = Vec::<(PathBuf, String, u64, u64, u64)>::new();
    for entry in entries {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().map(str::to_owned) else {
            report
                .preserved
                .push(target_relative_path(&file_name.to_string_lossy()));
            continue;
        };
        let relative = target_relative_path(&name);
        let path = entry.path();
        if !entry.file_type()?.is_dir() || !target_name_is_sweepable(&name) {
            report.preserved.push(relative);
            continue;
        }
        if protected_targets.contains(&relative) {
            report.governed_preserved.insert(relative.clone());
            report.preserved.push(relative.clone());
            report.refused.push(relative);
            continue;
        }
        let active = active_paths.targets.contains(&relative);
        let git_marker_or_probe_error = match fs::symlink_metadata(path.join(".git")) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(_) => true,
        };
        if git_marker_or_probe_error {
            if active {
                report.active_preserved.push(relative.clone());
                report.governed_preserved.insert(relative.clone());
                report.preserved.push(relative.clone());
            }
            report.refused.push(relative);
            continue;
        }
        if active {
            report.active_preserved.push(relative.clone());
            report.governed_preserved.insert(relative.clone());
            report.preserved.push(relative);
            continue;
        }
        if eligible_targets.is_some_and(|eligible| !eligible.contains(&relative)) {
            // Every public route requires a canonical terminal owner. A name
            // or TTL only narrows this set; it can never authorize an orphan.
            report.preserved.push(relative);
            continue;
        }
        if !old_enough(&path, ttl) {
            report.governed_preserved.insert(relative.clone());
            report.preserved.push(relative);
            continue;
        }

        let bytes = apparent_tree_bytes(&path)?;
        let identity = fs::symlink_metadata(&path)?;
        removals.push((path, relative, bytes, identity.dev(), identity.ino()));
    }

    report.active_worktrees = active_paths.worktrees.into_iter().collect();

    // All fallible discovery and measurement completes before the first deletion. Once mutation
    // starts, failures remain visible in the report instead of discarding earlier successes.
    for (path, relative, bytes, device, inode) in removals {
        let root_is_real = crate::reclaim::checked_storage_path(root, Path::new("orch/target"))
            .is_ok()
            && fs::symlink_metadata(&target_root)
                .map(|metadata| {
                    metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.dev() == target_metadata.dev()
                        && metadata.ino() == target_metadata.ino()
                })
                .unwrap_or(false);
        let entry_is_real = fs::symlink_metadata(&path)
            .map(|metadata| {
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.dev() == device
                    && metadata.ino() == inode
            })
            .unwrap_or(false);
        let git_marker_absent = matches!(
            fs::symlink_metadata(path.join(".git")),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        );
        let still_old_enough = old_enough(&path, ttl);
        if !root_is_real || !entry_is_real || !git_marker_absent || !still_old_enough {
            report.refused.push(relative.clone());
            report.failures.push(format!(
                "{relative}: deletion precondition changed after preflight"
            ));
            continue;
        }
        let removed = match crate::util::remove_dir_all_with_enotempty_retry(&path) {
            Ok(()) => true,
            Err(error) => {
                let disappeared = matches!(
                    fs::symlink_metadata(&path),
                    Err(stat_error) if stat_error.kind() == io::ErrorKind::NotFound
                );
                if !disappeared {
                    report.incomplete = true;
                    report.refused.push(relative.clone());
                    if let Ok(after) = apparent_tree_bytes(&path) {
                        let delta = bytes.saturating_sub(after);
                        report.removed_bytes.insert(relative.clone(), delta);
                        report.freed_bytes = report.freed_bytes.saturating_add(delta);
                    }
                    report
                        .failures
                        .push(format!("{relative}: remove stale target failed: {error}"));
                }
                disappeared
            }
        };
        if removed {
            report.removed.push(relative.clone());
            report.removed_bytes.insert(relative, bytes);
            report.freed_bytes = report.freed_bytes.saturating_add(bytes);
        }
    }
    Ok(report)
}

fn protected_target_paths_for_sites(
    events: &[EventRecord],
    protected_site_ids: &[String],
) -> Result<BTreeSet<String>> {
    let requested = protected_site_ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut found = BTreeSet::new();
    let mut targets = BTreeSet::new();
    for (index, event) in events.iter().enumerate() {
        if event.kind != "WorkspaceLeased" {
            continue;
        }
        let (site_id, lease) = parse_target_lease(index, event)?;
        if requested.contains(&site_id) {
            found.insert(site_id);
            if let Some(target) = lease.target {
                targets.insert(target);
            }
        }
    }
    let missing = requested.difference(&found).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "refused site ids have no canonical WorkspaceLeased target mapping: {}",
            missing.join(",")
        );
    }
    Ok(targets)
}

fn terminal_target_paths(events: &[EventRecord]) -> Result<BTreeSet<String>> {
    let active = active_target_paths(events)?;
    let mut terminal = BTreeSet::new();
    for (index, event) in events.iter().enumerate() {
        if event.kind != "WorkspaceLeased" {
            continue;
        }
        let (_, lease) = parse_target_lease(index, event)?;
        if let Some(target) = lease.target {
            if !active.targets.contains(&target) {
                terminal.insert(target);
            }
        }
    }
    Ok(terminal)
}

/// Read the current round ledger before sweeping. Missing/unreadable/corrupt ledgers are errors,
/// never an empty event set, because that would erase every Active lease from the evidence view.
pub fn sweep_targets_for_round(root: &Path, ttl: Duration) -> Result<SweepTargetsReport> {
    sweep_targets_for_round_with_policy(root, ttl, &[], false)
}

/// Round-close target sweep which keeps every target whose site reaper
/// returned a refusal.  This prevents a generic target sweep from deleting the
/// build half of a dirty review site after the lifecycle reaper retained its
/// worktree.
pub fn sweep_targets_for_round_preserving_sites(
    root: &Path,
    ttl: Duration,
    protected_site_ids: &[String],
) -> Result<SweepTargetsReport> {
    sweep_targets_for_round_with_policy(root, ttl, protected_site_ids, true)
}

fn sweep_targets_for_round_with_policy(
    root: &Path,
    ttl: Duration,
    protected_site_ids: &[String],
    _disposition_only: bool,
) -> Result<SweepTargetsReport> {
    let round = crate::current_round(root)?;
    crate::reclaim::with_maintenance_ledger(root, || {
        let inventory = crate::reclaim::storage_inventory(root);
        let selected = inventory
            .rounds
            .iter()
            .find(|r| r.round == round)
            .context("current target-sweep ledger unavailable")?;
        if let Some(error) = &selected.error {
            bail!("current target-sweep evidence invalid: {error}");
        }
        if selected.events.is_empty() {
            bail!("current target-sweep ledger is empty");
        }
        let mut protected = protected_target_paths_for_sites(&selected.events, protected_site_ids)?;
        let mut eligible = BTreeSet::new();
        for site in &selected.sites {
            let preview = crate::sites::maintain_one_site(root, &round, &selected.events, site, true);
            if crate::reclaim::ownership_refusal(root, &inventory, &round, site).is_some()
                || !preview.eligible
                || !preview.criteria.iter().any(|c| c.name == "last-user-absent" && c.passed == Some(true))
            {
                protected.insert(site.target.clone());
            } else {
                eligible.insert(site.target.clone());
            }
        }
        sweep_targets_preserving(root, &selected.events, ttl, &protected, Some(&eligible))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SlotKind {
    MainWorktree,
    TaskWorktree,
    FixedReviewRoot,
    TrialStaging,
}

impl SlotKind {
    /// Only detached trial staging roots may receive a target from the trial pool.
    pub fn may_share_trial_target(self) -> bool {
        matches!(self, Self::TrialStaging)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildIdentity {
    pub schema: u32,
    pub kind: SlotKind,
    pub canonical_source_root: String,
    pub canonical_common_dir: String,
    pub rustc_version: String,
    pub cargo_version: String,
    pub target_triple: String,
    pub cargo_lock_sha256: String,
    pub build_config_digest: String,
}

impl BuildIdentity {
    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }

    fn mismatch_reason(&self, other: &Self) -> String {
        let field = if self.schema != other.schema {
            "schema"
        } else if self.kind != other.kind {
            "kind"
        } else if self.canonical_common_dir != other.canonical_common_dir {
            "canonical_common_dir"
        } else if self.canonical_source_root != other.canonical_source_root {
            "canonical_source_root"
        } else if self.rustc_version != other.rustc_version {
            "rustc_version"
        } else if self.cargo_version != other.cargo_version {
            "cargo_version"
        } else if self.target_triple != other.target_triple {
            "target_triple"
        } else if self.cargo_lock_sha256 != other.cargo_lock_sha256 {
            "cargo_lock_sha256"
        } else if self.build_config_digest != other.build_config_digest {
            "build_config_digest"
        } else {
            "identity"
        };
        format!("build identity mismatch: {field}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDecision {
    ReuseIncremental,
    FreshGeneration { reason: String },
}

/// One member of the bounded trial slot pool.  The lock is held by [`with_trial_slot`] for the
/// complete worktree/merge/gate/cleanup sequence; this value cannot outlive that closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialSlot {
    index: usize,
    root: PathBuf,
    source_root: PathBuf,
}

impl TrialSlot {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn source_root(&self) -> &Path {
        &self.source_root
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildMarker {
    identity: BuildIdentity,
    canonical_target: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmCloneMethod {
    ApfsReflink,
}

impl WarmCloneMethod {
    fn label(self) -> &'static str {
        match self {
            Self::ApfsReflink => "apfs-reflink",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmPreparation {
    ReusedIncremental,
    Warmed {
        method: WarmCloneMethod,
        source: PathBuf,
        opened_because: String,
        invalidated_workspace_entries: usize,
        retained_dependency_entries: usize,
    },
    ColdFallback {
        reason: String,
    },
}

impl WarmPreparation {
    pub fn render(&self) -> String {
        match self {
            Self::ReusedIncremental => {
                "trial-cache reuse: identity marker matched; reusing private incremental target"
                    .to_string()
            }
            Self::Warmed {
                method,
                source,
                opened_because,
                invalidated_workspace_entries,
                retained_dependency_entries,
            } => format!(
                "trial-cache warm prepared: method={} source={} opened-because={} workspace-invalidated={} dependency-entries-retained={}",
                method.label(),
                source.display(),
                opened_because,
                invalidated_workspace_entries,
                retained_dependency_entries
            ),
            Self::ColdFallback { reason } => {
                format!("trial-cache fallback: cold private generation; reason={reason}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmStartAssessment {
    NotApplicable,
    Proven {
        workspace_crates: Vec<String>,
        dependency_rebuilds: Vec<String>,
    },
    FallbackRequired {
        reason: String,
        workspace_crates: Vec<String>,
        dependency_rebuilds: Vec<String>,
    },
}

impl WarmStartAssessment {
    pub fn requires_fallback(&self) -> bool {
        matches!(self, Self::FallbackRequired { .. })
    }

    pub fn render(&self) -> String {
        match self {
            Self::NotApplicable => "trial-cache warm proof: not applicable".to_string(),
            Self::Proven {
                workspace_crates,
                dependency_rebuilds,
            } => format!(
                "trial-cache warm proven: workspace-rebuilt={} dependency-rebuilds={}",
                render_list(workspace_crates),
                render_list(dependency_rebuilds)
            ),
            Self::FallbackRequired {
                reason,
                workspace_crates,
                dependency_rebuilds,
            } => format!(
                "trial-cache warm unproven; fallback required: reason={reason}; workspace-rebuilt={}; dependency-rebuilds={}",
                render_list(workspace_crates),
                render_list(dependency_rebuilds)
            ),
        }
    }
}

fn render_list(items: &[String]) -> String {
    if items.is_empty() {
        "<none>".to_string()
    } else {
        items.join(",")
    }
}

#[derive(Debug, Clone)]
pub struct PreparedTrialTarget {
    generation: u64,
    target_dir: PathBuf,
    marker_path: PathBuf,
    expected_marker: BuildMarker,
    warm: WarmPreparation,
}

impl PreparedTrialTarget {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn target_dir(&self) -> &Path {
        &self.target_dir
    }

    pub fn warm_preparation(&self) -> &WarmPreparation {
        &self.warm
    }

    /// Re-read the runtime-owned marker immediately before log creation and spawn.
    pub fn verify_before_spawn(&self) -> Result<()> {
        let bytes = fs::read(&self.marker_path).with_context(|| {
            format!(
                "trial cache identity refused before log/spawn: marker missing {}",
                self.marker_path.display()
            )
        })?;
        let marker: BuildMarker = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "trial cache identity refused before log/spawn: marker corrupt {}",
                self.marker_path.display()
            )
        })?;
        if marker != self.expected_marker {
            bail!(
                "trial cache identity refused before log/spawn: marker mismatch {}",
                self.marker_path.display()
            );
        }
        let canonical_target = fs::canonicalize(&self.target_dir).with_context(|| {
            format!(
                "trial cache identity refused before log/spawn: canonical target unavailable {}",
                self.target_dir.display()
            )
        })?;
        if canonical_target != marker.canonical_target {
            bail!("trial cache identity refused before log/spawn: canonical_target mismatch");
        }
        Ok(())
    }

    pub fn assess_gate_log(&self, exit_code: i32, log_path: &Path) -> Result<WarmStartAssessment> {
        if !matches!(self.warm, WarmPreparation::Warmed { .. }) {
            return Ok(WarmStartAssessment::NotApplicable);
        }
        let text = fs::read_to_string(log_path)
            .with_context(|| format!("read trial warm gate log failed: {}", log_path.display()))?;
        Ok(assess_warm_log(exit_code, &text))
    }
}

pub fn resolve_trial_timeout_secs(base: u64, explicit: Option<u64>) -> u64 {
    explicit.unwrap_or_else(|| base.checked_mul(3).unwrap_or(u64::MAX))
}

/// Hash the entire ordered gate command configuration.  Timeout values are included because they
/// are signed gate semantics even though they do not normally alter Cargo artifacts.
pub fn build_config_digest(
    gate_refs: &[String],
    commands: &BTreeMap<String, CommandSpec>,
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"orch-trial-build-config-v1\0");
    for gate_ref in gate_refs {
        let spec = commands
            .get(gate_ref)
            .with_context(|| format!("trial build config missing command: {gate_ref}"))?;
        hash_part(&mut digest, gate_ref.as_bytes());
        for arg in &spec.argv {
            hash_part(&mut digest, arg.as_bytes());
        }
        digest.update(spec.timeout_seconds.to_be_bytes());
        digest.update(spec.trial_timeout_seconds.unwrap_or(0).to_be_bytes());
    }
    Ok(hex::encode(digest.finalize()))
}

fn hash_part(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

/// Return the Cargo executable only when every trial gate is a Cargo command using the same
/// executable.  Synthetic/non-Rust gates retain the legacy one-shot scratch behavior.
pub fn trial_cargo_program(
    gate_refs: &[String],
    commands: &BTreeMap<String, CommandSpec>,
) -> Option<PathBuf> {
    let mut program: Option<PathBuf> = None;
    for gate_ref in gate_refs {
        let spec = commands.get(gate_ref)?;
        let candidate = PathBuf::from(spec.argv.first()?);
        if candidate.file_name().and_then(|name| name.to_str()) != Some("cargo") {
            return None;
        }
        if program.as_ref().is_some_and(|seen| seen != &candidate) {
            return None;
        }
        program = Some(candidate);
    }
    program
}

/// Hold one exclusive slot lock for a complete trial operation.  Available slots are scanned from
/// a task-stable starting point; if all are occupied we wait on that preferred slot instead of
/// creating an unbounded fifth cache root.
pub fn with_trial_slot<T>(
    repo_root: &Path,
    task_id: &str,
    action: impl FnOnce(&TrialSlot) -> Result<T>,
) -> Result<T> {
    let pool = repo_root.join(".cowork-temp/trial-cache/slots");
    fs::create_dir_all(&pool).context("create trial slot pool failed")?;
    let preferred = stable_slot_index(task_id);
    let mut action = Some(action);
    for offset in 0..TRIAL_SLOT_COUNT {
        let index = (preferred + offset) % TRIAL_SLOT_COUNT;
        let root = pool.join(format!("slot-{index:02}"));
        fs::create_dir_all(&root)
            .with_context(|| format!("create trial slot {} failed", root.display()))?;
        let file = open_slot_lock(&root)?;
        let mut lock = RwLock::new(file);
        match lock.try_write() {
            Ok(_guard) => {
                let slot = TrialSlot {
                    index,
                    source_root: root.join("source"),
                    root,
                };
                return action.take().expect("trial slot action consumed once")(&slot);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error).context("acquire trial slot lock failed"),
        };
    }

    let root = pool.join(format!("slot-{preferred:02}"));
    let file = open_slot_lock(&root)?;
    let mut lock = RwLock::new(file);
    let _guard = lock.write().context("wait for bounded trial slot failed")?;
    let slot = TrialSlot {
        index: preferred,
        source_root: root.join("source"),
        root,
    };
    action.take().expect("trial slot action consumed once")(&slot)
}

fn stable_slot_index(task_id: &str) -> usize {
    let digest = Sha256::digest(task_id.as_bytes());
    u16::from_be_bytes([digest[0], digest[1]]) as usize % TRIAL_SLOT_COUNT
}

fn open_slot_lock(root: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(root.join("slot.lock"))
        .with_context(|| format!("open trial slot lock failed: {}", root.display()))
}

/// Remove old private build generations while preserving each slot's newest `keep_latest`
/// generations within the default per-slot budget.  The stable `source/` worktree and `slot.lock`
/// live beside `generations/`, so the traversal never removes either of them.  Existing production
/// slot locks are acquired before deletion; fixture/legacy slots without a lock remain sweepable
/// without creating one.  The original report shape remains terse for existing callers; round
/// close uses [`sweep_trial_cache_with_budget`] to obtain per-slot visibility.
pub fn sweep_trial_cache(repo_root: &Path, keep_latest: usize) -> Result<TrialCacheSweepReport> {
    let mut report =
        sweep_trial_cache_with_budget(repo_root, keep_latest, DEFAULT_TRIAL_SLOT_BUDGET_BYTES)?;
    report.slot_usage.clear();
    Ok(report)
}

/// Remove generations until every slot retains at most `keep_latest` newest generations whose
/// cumulative logical size is within `slot_budget_bytes`.  An oversized newest generation is
/// therefore reclaimed even when it is the slot's only generation.
pub fn sweep_trial_cache_with_budget(
    repo_root: &Path,
    keep_latest: usize,
    slot_budget_bytes: u64,
) -> Result<TrialCacheSweepReport> {
    let slots_root = repo_root.join(".cowork-temp/trial-cache/slots");
    let entries = match fs::read_dir(&slots_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read trial cache slots failed: {}", slots_root.display())
            })
        }
    };

    let mut slots = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if file_type.is_dir() && name.to_str().is_some_and(|name| name.starts_with("slot-")) {
            slots.push(entry.path());
        }
    }
    slots.sort();

    let mut report = TrialCacheSweepReport::default();
    for slot in slots {
        let usage = sweep_trial_slot(&slot, keep_latest, slot_budget_bytes, &mut report)?;
        report.slot_usage.push(usage);
    }
    Ok(report)
}

fn sweep_trial_slot(
    slot: &Path,
    keep_latest: usize,
    slot_budget_bytes: u64,
    report: &mut TrialCacheSweepReport,
) -> Result<TrialCacheSlotUsage> {
    let lock_path = slot.join("slot.lock");
    match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => {
            let mut lock = RwLock::new(file);
            let _guard = lock.write().with_context(|| {
                format!(
                    "wait for trial cache sweep slot lock failed: {}",
                    lock_path.display()
                )
            })?;
            sweep_slot_generations(slot, keep_latest, slot_budget_bytes, report)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            sweep_slot_generations(slot, keep_latest, slot_budget_bytes, report)
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "open trial cache sweep slot lock failed: {}",
                lock_path.display()
            )
        }),
    }
}

fn sweep_slot_generations(
    slot: &Path,
    keep_latest: usize,
    slot_budget_bytes: u64,
    report: &mut TrialCacheSweepReport,
) -> Result<TrialCacheSlotUsage> {
    let logical_bytes_before = apparent_tree_bytes(slot)?;
    let generations_root = slot.join("generations");
    let entries = match fs::read_dir(&generations_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TrialCacheSlotUsage {
                slot: slot
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                logical_bytes_before,
                logical_bytes_after: logical_bytes_before,
            })
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "read trial cache generations failed: {}",
                    generations_root.display()
                )
            })
        }
    };
    let mut generations = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(number) = name.strip_prefix(GENERATION_PREFIX) else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        if name != format!("{GENERATION_PREFIX}{number:06}") {
            continue;
        }
        let path = entry.path();
        let bytes = apparent_tree_bytes(&path)?;
        generations.push((number, path, bytes));
    }
    generations.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut remove_count = generations.len().saturating_sub(keep_latest);
    let mut retained_bytes = generations
        .iter()
        .skip(remove_count)
        .fold(0u64, |total, (_, _, bytes)| total.saturating_add(*bytes));
    while retained_bytes > slot_budget_bytes && remove_count < generations.len() {
        retained_bytes = retained_bytes.saturating_sub(generations[remove_count].2);
        remove_count += 1;
    }

    for (_, generation, bytes) in generations.into_iter().take(remove_count) {
        crate::util::remove_dir_all_with_enotempty_retry(&generation).with_context(|| {
            format!(
                "remove old trial cache generation failed: {}",
                generation.display()
            )
        })?;
        report.removed = report.removed.saturating_add(1);
        report.freed_bytes = report.freed_bytes.saturating_add(bytes);
    }
    Ok(TrialCacheSlotUsage {
        slot: slot
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        logical_bytes_before,
        logical_bytes_after: apparent_tree_bytes(slot)?,
    })
}

pub(crate) fn apparent_tree_bytes(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("stat trial cache entry failed: {}", path.display()))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            for entry in fs::read_dir(&path)
                .with_context(|| format!("read trial cache entry failed: {}", path.display()))?
            {
                pending.push(entry?.path());
            }
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

pub fn inspect_build_identity(
    source_root: &Path,
    cargo_program: &Path,
    build_config_digest: String,
) -> Result<BuildIdentity> {
    let canonical_source_root = fs::canonicalize(source_root)
        .with_context(|| {
            format!(
                "canonicalize trial source failed: {}",
                source_root.display()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let canonical_common_dir = canonical_git_path(source_root, "--git-common-dir")?
        .to_string_lossy()
        .into_owned();
    let cargo_version = tool_output(cargo_program, &["--version", "--verbose"])
        .or_else(|_| tool_output(cargo_program, &["--version"]))?;
    let rustc_program = sibling_rustc(cargo_program);
    let rustc_verbose = tool_output(&rustc_program, &["-vV"])?;
    let rustc_version = rustc_verbose.lines().next().unwrap_or_default().to_string();
    let target_triple = rustc_verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("rustc -vV missing host target triple")?
        .to_string();
    let cargo_lock = cargo_lock_path(source_root)?;
    let cargo_lock_sha256 = hex::encode(Sha256::digest(
        fs::read(&cargo_lock)
            .with_context(|| format!("read Cargo.lock failed: {}", cargo_lock.display()))?,
    ));
    Ok(BuildIdentity {
        schema: BUILD_IDENTITY_SCHEMA,
        kind: SlotKind::TrialStaging,
        canonical_source_root,
        canonical_common_dir,
        rustc_version,
        cargo_version,
        target_triple,
        cargo_lock_sha256,
        build_config_digest,
    })
}

fn sibling_rustc(cargo_program: &Path) -> PathBuf {
    let sibling = cargo_program.with_file_name("rustc");
    if sibling.is_file() {
        sibling
    } else {
        PathBuf::from("rustc")
    }
}

fn tool_output(program: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("run tool identity probe failed: {}", program.display()))?;
    if !output.status.success() {
        bail!(
            "tool identity probe failed: {} {:?}: {}",
            program.display(),
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8(output.stdout).context("tool identity probe returned non-UTF8")?;
    Ok(text.trim().to_string())
}

fn canonical_git_path(root: &Path, arg: &str) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", arg])
        .output()
        .context("git rev-parse identity probe failed")?;
    if !output.status.success() {
        bail!(
            "git rev-parse {arg} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = String::from_utf8(output.stdout)
        .context("git rev-parse identity path returned non-UTF8")?;
    let raw = raw.trim();
    if raw.is_empty() || raw.lines().count() != 1 {
        bail!("git rev-parse {arg} returned empty/multiline path");
    }
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    fs::canonicalize(&absolute).with_context(|| {
        format!(
            "canonicalize git identity path failed: {}",
            absolute.display()
        )
    })
}

fn cargo_lock_path(source_root: &Path) -> Result<PathBuf> {
    for candidate in [
        source_root.join("Cargo.lock"),
        source_root.join("orch/Cargo.lock"),
    ] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("trial source has no Cargo.lock: {}", source_root.display())
}

pub fn prepare_trial_target(
    slot: &TrialSlot,
    identity: &BuildIdentity,
    hot_target: Option<&Path>,
) -> Result<PreparedTrialTarget> {
    if identity.kind != SlotKind::TrialStaging || !identity.kind.may_share_trial_target() {
        bail!("only TrialStaging identity may use the trial slot pool");
    }
    let generations = slot.root.join("generations");
    fs::create_dir_all(&generations).context("create trial generations directory failed")?;
    let latest = latest_generation(&generations)?;
    if let Some(generation) = latest {
        let target = generation_target(&generations, generation);
        match inspect_generation(&target, identity) {
            GenerationState::Reusable(marker) => {
                return Ok(prepared(
                    generation,
                    target,
                    marker,
                    WarmPreparation::ReusedIncremental,
                ));
            }
            GenerationState::Empty => {
                return initialize_generation(
                    &generations,
                    generation,
                    identity,
                    hot_target,
                    "empty generation",
                );
            }
            GenerationState::Refused(reason) => {
                return initialize_generation(
                    &generations,
                    generation.saturating_add(1),
                    identity,
                    hot_target,
                    &reason,
                );
            }
        }
    }
    initialize_generation(&generations, 0, identity, hot_target, "first generation")
}

pub fn prepare_cold_fallback_target(
    slot: &TrialSlot,
    identity: &BuildIdentity,
    reason: impl Into<String>,
) -> Result<PreparedTrialTarget> {
    let generations = slot.root.join("generations");
    fs::create_dir_all(&generations).context("create trial generations directory failed")?;
    let generation = latest_generation(&generations)?
        .map(|value| value.saturating_add(1))
        .unwrap_or(0);
    initialize_cold_generation(&generations, generation, identity, reason.into())
}

enum GenerationState {
    Empty,
    Reusable(BuildMarker),
    Refused(String),
}

fn inspect_generation(target: &Path, incoming: &BuildIdentity) -> GenerationState {
    let meta = match fs::symlink_metadata(target) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return GenerationState::Empty,
        Err(error) => return GenerationState::Refused(format!("target metadata error: {error}")),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return GenerationState::Refused("target is not a real directory".to_string());
    }
    let mut entries = match fs::read_dir(target) {
        Ok(entries) => entries,
        Err(error) => return GenerationState::Refused(format!("target read error: {error}")),
    };
    if entries.next().is_none() {
        return GenerationState::Empty;
    }
    let marker_path = target.join(MARKER_FILE);
    let bytes = match fs::read(&marker_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return GenerationState::Refused("non-empty target missing marker".to_string())
        }
        Err(error) => return GenerationState::Refused(format!("marker read error: {error}")),
    };
    let marker: BuildMarker = match serde_json::from_slice(&bytes) {
        Ok(marker) => marker,
        Err(error) => return GenerationState::Refused(format!("marker corrupt: {error}")),
    };
    if let CacheDecision::FreshGeneration { reason } =
        TrialSlot::decide(Some(&marker.identity), incoming)
    {
        return GenerationState::Refused(reason);
    }
    let canonical_target = match fs::canonicalize(target) {
        Ok(path) => path,
        Err(error) => return GenerationState::Refused(format!("canonical target error: {error}")),
    };
    if marker.canonical_target != canonical_target {
        return GenerationState::Refused("build identity mismatch: canonical_target".to_string());
    }
    GenerationState::Reusable(marker)
}

fn initialize_generation(
    generations: &Path,
    generation: u64,
    identity: &BuildIdentity,
    hot_target: Option<&Path>,
    reason: &str,
) -> Result<PreparedTrialTarget> {
    let mut generation = generation;
    let target = generation_target(generations, generation);
    fs::create_dir_all(&target)
        .with_context(|| format!("create trial target failed: {}", target.display()))?;
    if directory_nonempty(&target) {
        return initialize_generation(
            generations,
            generation.saturating_add(1),
            identity,
            hot_target,
            &format!("{reason}; candidate generation already non-empty"),
        );
    }

    let warm = match hot_target.filter(|path| path.is_dir() && directory_nonempty(path)) {
        Some(source) => match clone_hot_target(source, &target) {
            Ok(method) => {
                let invalidation = invalidate_workspace_artifacts(&target)?;
                if invalidation.invalidated_workspace_entries == 0
                    || invalidation.retained_dependency_entries == 0
                {
                    generation = generation.saturating_add(1);
                    return initialize_cold_generation(
                        generations,
                        generation,
                        identity,
                        format!(
                            "warm preflight unproven after {reason}: workspace-invalidated={} dependency-entries-retained={}",
                            invalidation.invalidated_workspace_entries,
                            invalidation.retained_dependency_entries
                        ),
                    );
                }
                WarmPreparation::Warmed {
                    method,
                    source: source.to_path_buf(),
                    opened_because: reason.to_string(),
                    invalidated_workspace_entries: invalidation.invalidated_workspace_entries,
                    retained_dependency_entries: invalidation.retained_dependency_entries,
                }
            }
            Err(error) => {
                generation = generation.saturating_add(1);
                return initialize_cold_generation(
                    generations,
                    generation,
                    identity,
                    format!("warm clone unavailable after {reason}: {error:#}"),
                );
            }
        },
        None => WarmPreparation::ColdFallback {
            reason: format!("hot target unavailable after {reason}"),
        },
    };
    write_new_marker(generation, target, identity, warm)
}

fn initialize_cold_generation(
    generations: &Path,
    generation: u64,
    identity: &BuildIdentity,
    reason: String,
) -> Result<PreparedTrialTarget> {
    let target = generation_target(generations, generation);
    fs::create_dir_all(&target)
        .with_context(|| format!("create cold trial target failed: {}", target.display()))?;
    if directory_nonempty(&target) {
        return initialize_cold_generation(
            generations,
            generation.saturating_add(1),
            identity,
            format!("{reason}; candidate generation already non-empty"),
        );
    }
    write_new_marker(
        generation,
        target,
        identity,
        WarmPreparation::ColdFallback { reason },
    )
}

fn write_new_marker(
    generation: u64,
    target: PathBuf,
    identity: &BuildIdentity,
    warm: WarmPreparation,
) -> Result<PreparedTrialTarget> {
    let canonical_target = fs::canonicalize(&target)
        .with_context(|| format!("canonicalize new trial target failed: {}", target.display()))?;
    let marker = BuildMarker {
        identity: identity.clone(),
        canonical_target,
    };
    let marker_path = target.join(MARKER_FILE);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker_path)
        .with_context(|| {
            format!(
                "refuse to overwrite trial marker: {}",
                marker_path.display()
            )
        })?;
    let bytes = serde_json::to_vec_pretty(&marker).context("serialize trial marker failed")?;
    file.write_all(&bytes)
        .context("write trial marker failed")?;
    file.write_all(b"\n")
        .context("finish trial marker failed")?;
    file.sync_all().context("sync trial marker failed")?;
    Ok(prepared(generation, target, marker, warm))
}

fn prepared(
    generation: u64,
    target_dir: PathBuf,
    marker: BuildMarker,
    warm: WarmPreparation,
) -> PreparedTrialTarget {
    PreparedTrialTarget {
        generation,
        marker_path: target_dir.join(MARKER_FILE),
        target_dir,
        expected_marker: marker,
        warm,
    }
}

fn latest_generation(generations: &Path) -> Result<Option<u64>> {
    let mut latest: Option<u64> = None;
    for entry in fs::read_dir(generations)
        .with_context(|| format!("read trial generations failed: {}", generations.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(number) = name.strip_prefix(GENERATION_PREFIX) else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        latest = Some(latest.map_or(number, |seen| seen.max(number)));
    }
    Ok(latest)
}

fn generation_target(generations: &Path, generation: u64) -> PathBuf {
    generations
        .join(format!("{GENERATION_PREFIX}{generation:06}"))
        .join("target")
}

fn directory_nonempty(path: &Path) -> bool {
    fs::read_dir(path)
        .ok()
        .and_then(|mut entries| entries.next())
        .is_some()
}

fn clone_hot_target(source: &Path, target: &Path) -> Result<WarmCloneMethod> {
    let source_contents = source.join(".");
    let reflink = Command::new("cp")
        .arg("-c")
        .arg("-R")
        .arg(&source_contents)
        .arg(target)
        .output()
        .context("launch APFS reflink copy failed")?;
    if reflink.status.success() {
        return Ok(WarmCloneMethod::ApfsReflink);
    }

    // A failed clone may have left a partial tree.  Preserve it as evidence and let the caller
    // open a different cold generation rather than cleaning or overwriting this one in place.
    bail!(
        "cp -c failed (exit={}): {}",
        reflink.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&reflink.stderr).trim()
    )
}

struct InvalidationSummary {
    invalidated_workspace_entries: usize,
    retained_dependency_entries: usize,
}

fn invalidate_workspace_artifacts(target: &Path) -> Result<InvalidationSummary> {
    let debug = target.join("debug");
    let mut invalidated = 0usize;
    invalidated += remove_matching_entries(&debug.join(".fingerprint"), |name, _| {
        name.starts_with("orch-") || name.starts_with("orch_")
    })?;
    invalidated += remove_matching_entries(&debug.join("deps"), |name, kind| {
        kind.is_file()
            && name.starts_with("liborch_")
            && (name.ends_with(".rmeta") || name.ends_with(".rlib"))
    })?;
    invalidated += remove_matching_entries(&debug.join("incremental"), |name, _| {
        name.starts_with("orch-") || name.starts_with("orch_")
    })?;
    let retained_dependency_entries = fs::read_dir(debug.join(".fingerprint"))
        .map(|entries| entries.filter_map(std::result::Result::ok).count())
        .unwrap_or(0);
    Ok(InvalidationSummary {
        invalidated_workspace_entries: invalidated,
        retained_dependency_entries,
    })
}

fn remove_matching_entries(
    dir: &Path,
    predicate: impl Fn(&str, &fs::FileType) -> bool,
) -> Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read warm target directory failed: {}", dir.display()))
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !predicate(&name, &file_type) {
            continue;
        }
        if file_type.is_dir() {
            crate::util::remove_dir_all_with_enotempty_retry(&entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
        removed += 1;
    }
    Ok(removed)
}

fn assess_warm_log(exit_code: i32, text: &str) -> WarmStartAssessment {
    let mut workspace = Vec::new();
    let mut dependencies = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Compiling ") else {
            continue;
        };
        let name = rest.split_whitespace().next().unwrap_or_default();
        if name.starts_with("orch-") {
            push_unique(&mut workspace, name);
        } else if !name.is_empty() {
            push_unique(&mut dependencies, name);
        }
    }
    let has_core = workspace.iter().any(|name| name == "orch-core");
    let has_host = workspace.iter().any(|name| name == "orch-host");
    let reason = if exit_code != 0 {
        Some(format!("warm gate exited {exit_code}"))
    } else if !has_core || !has_host {
        Some("warm gate did not prove orch-core and orch-host rebuilt".to_string())
    } else if !dependencies.is_empty() {
        Some("warm gate rebuilt registry dependencies".to_string())
    } else {
        None
    };
    match reason {
        Some(reason) => WarmStartAssessment::FallbackRequired {
            reason,
            workspace_crates: workspace,
            dependency_rebuilds: dependencies,
        },
        None => WarmStartAssessment::Proven {
            workspace_crates: workspace,
            dependency_rebuilds: dependencies,
        },
    }
}

fn push_unique(items: &mut Vec<String>, value: &str) {
    if !items.iter().any(|item| item == value) {
        items.push(value.to_string());
    }
}

/// A foreground Cargo diagnostic whose generated target is owned by one invocation.
/// The repository and cwd must be clean Git roots; commands cannot override the private target.
#[derive(Debug, Clone)]
pub struct DiagnosticRequest {
    /// Absolute primary or linked worktree root inside the project.
    pub cwd: PathBuf,
    /// The absolute Cargo executable declared by the project's seedTargets command.
    pub executable: PathBuf,
    /// Cargo build/check/test/doc/clippy arguments, including locked manifest selection.
    pub args: Vec<String>,
    /// A nonempty, single-line explanation retained with the execution evidence.
    pub purpose: String,
}

/// Read-only observation or result for one diagnostic cache. Logical bytes are not physical space.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticCacheView {
    /// Runtime-generated cache identity, never an arbitrary deletion path.
    pub id: String,
    /// Derived private target path (or retiring path during interrupted deletion).
    pub path: PathBuf,
    /// removed, held, or failed; status inspection never implies a new deletion.
    pub disposition: String,
    /// Concrete eligibility, retention, or failure explanation.
    pub reason: String,
    /// Current no-follow logical size; unavailable measurements are null, not zero.
    pub logical_bytes: Option<u64>,
    /// Logical bytes removed by this operation only; a replay reports zero.
    pub removed_logical_bytes: u64,
    /// Whether a fresh sweep may attempt completion; preview is not authorization.
    pub eligible: bool,
    /// Actual child exit code, or null if normal exit was not observed.
    pub command_exit: Option<i32>,
    /// Durable record and stdout/stderr directory, retained outside the disposable target.
    pub evidence_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiagnosticLog {
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiagnosticRecord {
    schema_version: u32,
    id: String,
    root: PathBuf,
    cwd: PathBuf,
    head: String,
    executable: PathBuf,
    executable_sha256: String,
    args: Vec<String>,
    purpose: String,
    device: u64,
    inode: u64,
    phase: String,
    pid: Option<u32>,
    exit_code: Option<i32>,
    stdout: Option<DiagnosticLog>,
    stderr: Option<DiagnosticLog>,
    #[serde(default)]
    removal_inferred_from_absence: bool,
}

const DIAGNOSTIC_ROOT: &str = "coordination/runtime/diagnostic-cache";
const DIAGNOSTIC_RECORD_LIMIT: u64 = 1024 * 1024;

fn diagnostic_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() || root.to_str().is_none() || fs::canonicalize(root)? != root {
        bail!("diagnostic root must be canonical, absolute and UTF-8");
    }
    let marker = fs::symlink_metadata(root.join(".git"))
        .context("diagnostic root requires its own .git; never search ancestors")?;
    if marker.file_type().is_symlink() || !(marker.is_dir() || marker.is_file()) {
        bail!("unsafe diagnostic Git marker");
    }
    Ok(root.to_path_buf())
}

fn diagnostic_directory(root: &Path, relative: &Path, create: bool) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::PermissionsExt;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            bail!("diagnostic path is not a normal relative path");
        };
        path.push(part);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
                match fs::create_dir(&path) {
                    Ok(()) => fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                fs::symlink_metadata(&path)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("unsafe diagnostic directory: {}", path.display());
        }
    }
    Ok(Some(path))
}

fn diagnostic_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::MetadataExt;
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.file_type().is_symlink() {
        bail!(
            "diagnostic evidence is not a regular file: {}",
            path.display()
        );
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        bail!("diagnostic evidence identity changed while opening");
    }
    Ok(file)
}

fn diagnostic_hash(path: &Path) -> Result<DiagnosticLog> {
    use std::io::Read;
    let mut file = diagnostic_file(path)?;
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        bytes = bytes
            .checked_add(n as u64)
            .context("diagnostic log length overflow")?;
    }
    let after = file.metadata()?;
    if before.len() != bytes || after.len() != bytes || before.modified()? != after.modified()? {
        bail!("diagnostic log is still changing");
    }
    Ok(DiagnosticLog {
        sha256: hex::encode(hash.finalize()),
        bytes,
    })
}

fn diagnostic_read(dir: &Path) -> Result<DiagnosticRecord> {
    use std::io::Read;
    let mut file = diagnostic_file(&dir.join("record.json"))?;
    if file.metadata()?.len() > DIAGNOSTIC_RECORD_LIMIT {
        bail!("diagnostic record exceeds bound");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(DIAGNOSTIC_RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > DIAGNOSTIC_RECORD_LIMIT {
        bail!("diagnostic record exceeds bound");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn diagnostic_save(dir: &Path, record: &DiagnosticRecord) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join("record.json");
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let _ = diagnostic_file(&path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temp = dir.join(format!(".record-{}.tmp", ulid::Ulid::new()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, &path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn diagnostic_git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    if !output.status.success() {
        bail!("diagnostic Git observation failed");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn diagnostic_cwd(root: &Path, cwd: &Path) -> Result<String> {
    if !cwd.is_absolute() || !cwd.starts_with(root) || fs::canonicalize(cwd)? != cwd {
        bail!("diagnostic cwd must be an in-project canonical absolute worktree root");
    }
    diagnostic_root(cwd)?;
    if diagnostic_git(cwd, &["rev-parse", "--show-toplevel"])? != cwd.to_string_lossy()
        || !crate::gitx::same_common_dir(root, cwd)?
    {
        bail!("diagnostic cwd is not a primary/linked root of this repository");
    }
    if !diagnostic_git(cwd, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
        bail!("diagnostic cwd is dirty; commit or preserve changes before a fixed-HEAD run");
    }
    let head = diagnostic_git(cwd, &["rev-parse", "HEAD^{commit}"])?;
    if !full_sha(&head) {
        bail!("diagnostic HEAD is not a full commit SHA");
    }
    Ok(head)
}

fn diagnostic_validate_request(root: &Path, request: &DiagnosticRequest) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    if !request.executable.is_absolute() {
        bail!("diagnostic executable must be absolute");
    }
    if request.purpose.trim().is_empty() || request.purpose.chars().any(char::is_control) {
        bail!("diagnostic purpose must be a nonempty single line");
    }
    diagnostic_root(root)?;
    crate::reclaim::validate_storage_root(root)?;
    diagnostic_directory(root, Path::new(DIAGNOSTIC_ROOT), false)?;
    let binding = crate::binding::load(root)?;
    let pinned = binding
        .commands
        .get("seedTargets")
        .and_then(|command| command.argv.first())
        .context("diagnostic Cargo tool is not declared by seedTargets")?;
    if request.executable != Path::new(pinned)
        || request.executable.file_name().and_then(|s| s.to_str()) != Some("cargo")
    {
        bail!("diagnostics accept only the project's explicit Cargo executable, not provider/remote jobs");
    }
    let executable = fs::metadata(&request.executable)?;
    if !executable.is_file() || executable.permissions().mode() & 0o111 == 0 {
        bail!("diagnostic Cargo executable is not executable/regular");
    }
    if !matches!(
        request.args.first().map(String::as_str),
        Some("build" | "check" | "test" | "doc" | "clippy")
    ) || !request.args.iter().any(|arg| arg == "--locked")
    {
        bail!("diagnostic requires a local Cargo build/check/test/doc/clippy with --locked");
    }
    if request.args.iter().any(|arg| {
        arg == "--target-dir"
            || arg.starts_with("--target-dir=")
            || arg == "--config"
            || arg.starts_with("--config=")
            || arg == "--fix"
    }) {
        bail!("diagnostic target/config/source-rewrite overrides are forbidden");
    }
    let mut manifests = Vec::new();
    for (index, arg) in request.args.iter().enumerate() {
        if arg == "--manifest-path" {
            manifests.push(
                request
                    .args
                    .get(index + 1)
                    .context("manifest path missing")?
                    .as_str(),
            );
        } else if let Some(path) = arg.strip_prefix("--manifest-path=") {
            manifests.push(path);
        }
    }
    if manifests.len() != 1 {
        bail!("diagnostic requires exactly one --manifest-path");
    }
    let head = diagnostic_cwd(root, &request.cwd)?;
    let manifest = fs::canonicalize(request.cwd.join(manifests[0]))?;
    if !manifest.starts_with(&request.cwd) || !manifest.is_file() {
        bail!("manifest escapes diagnostic cwd");
    }
    Ok(head)
}

fn diagnostic_registration(root: &Path, dir: &Path, record: &DiagnosticRecord) -> Result<()> {
    if record.schema_version != 1
        || record.root != root
        || record
            .id
            .parse::<ulid::Ulid>()
            .ok()
            .is_none_or(|id| id.to_string() != record.id)
        || dir != root.join(DIAGNOSTIC_ROOT).join(&record.id)
        || !full_sha(&record.head)
        || record.executable_sha256.len() != 64
        || !record
            .executable_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !record.cwd.is_absolute()
        || !record.cwd.starts_with(root)
        || !matches!(
            record.phase.as_str(),
            "registered"
                | "running"
                | "exited"
                | "terminal"
                | "removing"
                | "removed"
                | "interrupted"
        )
    {
        bail!("diagnostic registration identity is invalid");
    }
    diagnostic_directory(
        root,
        Path::new(DIAGNOSTIC_ROOT).join(&record.id).as_path(),
        false,
    )?
    .context("diagnostic record directory disappeared")?;
    Ok(())
}

fn diagnostic_identity(root: &Path, dir: &Path, record: &DiagnosticRecord) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    diagnostic_registration(root, dir, record)?;
    let target = dir.join("target");
    let retiring = dir.join("retiring");
    if fs::symlink_metadata(&target).is_ok() && fs::symlink_metadata(&retiring).is_ok() {
        bail!("diagnostic target and retiring path both exist");
    }
    let path = if record.phase == "removing" && fs::symlink_metadata(&retiring).is_ok() {
        retiring
    } else {
        target
    };
    let m = fs::symlink_metadata(&path)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.dev() != record.device
        || m.ino() != record.inode
    {
        bail!("diagnostic target directory identity changed; preserve replacement");
    }
    Ok(path)
}

trait DiagnosticProbe {
    fn quiet(&self, dir: &Path, target: &Path, pid: u32) -> Result<()>;
}

struct DiagnosticKernelProbe;
impl DiagnosticProbe for DiagnosticKernelProbe {
    fn quiet(&self, dir: &Path, target: &Path, pid: u32) -> Result<()> {
        if pid == 0 {
            bail!("diagnostic lacks a captured process group");
        }
        let processes = Command::new("/bin/ps").args(["-axo", "pgid="]).output()?;
        if !processes.status.success() {
            bail!("diagnostic process observation failed");
        }
        for line in std::str::from_utf8(&processes.stdout)?
            .lines()
            .filter(|s| !s.trim().is_empty())
        {
            if line.trim().parse::<u32>()? == pid {
                bail!("diagnostic process group still present; no signals sent");
            }
        }
        let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
            .context("lsof unavailable; cache ownership remains unknown")?;
        for recursive in [true, false] {
            let mut command = Command::new(lsof);
            command.args(["-nP", "-t"]);
            if recursive {
                command.arg("+D").arg(target);
            } else {
                command
                    .arg("--")
                    .arg(dir.join("stdout.log"))
                    .arg(dir.join("stderr.log"));
            }
            let output = command.output()?;
            if output.status.code() != Some(1)
                || !output.stdout.is_empty()
                || !output.stderr.is_empty()
            {
                bail!("diagnostic files open or ownership observation uncertain; retained");
            }
        }
        Ok(())
    }
}

fn diagnostic_eligible(
    root: &Path,
    dir: &Path,
    record: &DiagnosticRecord,
    probe: &dyn DiagnosticProbe,
) -> Result<PathBuf> {
    if !matches!(record.phase.as_str(), "exited" | "terminal" | "removing")
        || record.exit_code.is_none()
    {
        bail!("no durable child wait result; running/crashed/unknown diagnostic retained");
    }
    if fs::symlink_metadata(dir.join(".keep")).is_ok() {
        bail!("explicit debug .keep marker; retained");
    }
    let target = diagnostic_identity(root, dir, record)?;
    if diagnostic_cwd(root, &record.cwd)? != record.head {
        bail!("diagnostic source HEAD changed; retained");
    }
    probe.quiet(
        dir,
        &target,
        record.pid.context("diagnostic has no child identity")?,
    )?;
    for (name, expected) in [
        ("stdout.log", &record.stdout),
        ("stderr.log", &record.stderr),
    ] {
        let actual = diagnostic_hash(&dir.join(name))?;
        if let Some(expected) = expected {
            if actual.sha256 != expected.sha256 || actual.bytes != expected.bytes {
                bail!("diagnostic stable log digest changed; retained");
            }
        } else if record.phase != "exited" {
            bail!("diagnostic terminal lacks stable log evidence");
        }
    }
    Ok(target)
}

fn diagnostic_view(
    dir: &Path,
    record: Option<&DiagnosticRecord>,
    disposition: &str,
    reason: String,
) -> DiagnosticCacheView {
    let safe_dir =
        fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
    let path = if safe_dir && fs::symlink_metadata(dir.join("retiring")).is_ok() {
        dir.join("retiring")
    } else {
        dir.join("target")
    };
    let logical_bytes = if !safe_dir {
        None
    } else {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => apparent_tree_bytes(&path).ok(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(0),
            _ => None,
        }
    };
    DiagnosticCacheView {
        id: dir.file_name().unwrap_or_default().to_string_lossy().into(),
        path,
        disposition: disposition.into(),
        reason,
        logical_bytes,
        removed_logical_bytes: 0,
        eligible: false,
        command_exit: record.and_then(|r| r.exit_code),
        evidence_dir: dir.to_path_buf(),
    }
}

fn diagnostic_absence_receipt_ready(dir: &Path, record: &DiagnosticRecord) -> Result<bool> {
    if record.phase != "removing" || !["target", "retiring"].iter().all(|name| matches!(
        fs::symlink_metadata(dir.join(name)), Err(error) if error.kind() == io::ErrorKind::NotFound
    )) { return Ok(false); }
    if record.exit_code.is_none() || record.pid.is_none_or(|pid| pid == 0) {
        bail!("absent diagnostic target lacks a durable child wait identity");
    }
    for (name, expected) in [
        ("stdout.log", &record.stdout),
        ("stderr.log", &record.stderr),
    ] {
        let expected = expected
            .as_ref()
            .context("absent diagnostic target lacks stable log evidence")?;
        let actual = diagnostic_hash(&dir.join(name))?;
        if actual.sha256 != expected.sha256 || actual.bytes != expected.bytes {
            bail!("absent diagnostic target log digest changed; receipt retained");
        }
    }
    Ok(true)
}

fn diagnostic_reclaim(
    root: &Path,
    dir: &Path,
    record: &mut DiagnosticRecord,
    probe: &dyn DiagnosticProbe,
) -> Result<DiagnosticCacheView> {
    use std::os::unix::fs::MetadataExt;
    diagnostic_registration(root, dir, record)?;
    if record.phase == "removed" {
        let absent = ["target", "retiring"].iter().all(|p| matches!(fs::symlink_metadata(dir.join(p)), Err(e) if e.kind()==io::ErrorKind::NotFound));
        return Ok(diagnostic_view(dir, Some(record), if absent {"removed"} else {"held"},
            if absent && record.removal_inferred_from_absence {"absence observed after removing receipt; original remover and bytes unknown; no new deletion"} else if absent {"already removed; no bytes counted again"} else {"path reappeared after removal; not owned"}.into()));
    }
    // Recover only the persisted receipt; absence never invents a deletion or its byte count.
    if diagnostic_absence_receipt_ready(dir, record)? {
        record.phase = "removed".into();
        record.removal_inferred_from_absence = true;
        diagnostic_save(dir, record)?;
        return Ok(diagnostic_view(dir, Some(record), "removed",
            "absence observed after removing receipt; original remover and bytes unknown; no new deletion".into()));
    }
    // Preserve stable output even when explicit debug retention prevents deletion.
    if record.phase == "exited" {
        let target = diagnostic_identity(root, dir, record)?;
        if let Err(error) = probe.quiet(dir, &target, record.pid.context("missing child identity")?)
        {
            return Ok(diagnostic_view(
                dir,
                Some(record),
                "held",
                format!("{error:#}"),
            ));
        }
        diagnostic_file(&dir.join("stdout.log"))?.sync_all()?;
        diagnostic_file(&dir.join("stderr.log"))?.sync_all()?;
        record.stdout = Some(diagnostic_hash(&dir.join("stdout.log"))?);
        record.stderr = Some(diagnostic_hash(&dir.join("stderr.log"))?);
        record.phase = "terminal".into();
        diagnostic_save(dir, record)?;
    }
    let target = match diagnostic_eligible(root, dir, record, probe) {
        Ok(path) => path,
        Err(error) => {
            return Ok(diagnostic_view(
                dir,
                Some(record),
                "held",
                format!("{error:#}"),
            ))
        }
    };
    let before = apparent_tree_bytes(&target)?;
    record.phase = "removing".into();
    diagnostic_save(dir, record)?;
    let retiring = dir.join("retiring");
    if target != retiring {
        diagnostic_identity(root, dir, record)?;
        fs::rename(&target, &retiring)?;
        File::open(dir)?.sync_all()?;
    }
    let moved = fs::symlink_metadata(&retiring)?;
    if moved.file_type().is_symlink()
        || !moved.is_dir()
        || moved.dev() != record.device
        || moved.ino() != record.inode
    {
        bail!("renamed diagnostic identity differs; no deletion performed");
    }
    probe.quiet(dir, &retiring, record.pid.context("missing child group")?)?;
    if let Err(error) = fs::remove_dir_all(&retiring) {
        let mut view = diagnostic_view(
            dir,
            Some(record),
            "failed",
            format!("partial deletion retained for retry: {error}"),
        );
        if let Some(after) = view.logical_bytes {
            view.removed_logical_bytes = before.saturating_sub(after);
        }
        return Ok(view);
    }
    File::open(dir)?.sync_all()?;
    record.phase = "removed".into();
    diagnostic_save(dir, record)?;
    let mut view = diagnostic_view(
        dir,
        Some(record),
        "removed",
        "normal child exit, stable evidence and quiescent owned target".into(),
    );
    view.removed_logical_bytes = before;
    Ok(view)
}

/// Run one local Cargo diagnostic with a private target, durable pre-spawn registration and logs.
/// No provider job or process termination is performed. Interrupted/unknown ownership stays held.
/// Execution is independent of a round/IR; clean fixed source and real space admission remain required.
pub fn run_managed_diagnostic(
    root: &Path,
    request: &DiagnosticRequest,
) -> Result<DiagnosticCacheView> {
    run_managed_diagnostic_with_retention(root, request, false)
}

/// Run the same diagnostic while optionally placing an explicit `.keep` debug marker.
/// Removing that marker later only clears retention preference; sweep still verifies ownership.
pub fn run_managed_diagnostic_with_retention(
    root: &Path,
    request: &DiagnosticRequest,
    keep: bool,
) -> Result<DiagnosticCacheView> {
    use std::os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    };
    let head = diagnostic_validate_request(root, request)?;
    crate::close::with_protocol_ledger_effect(root, "managed Cargo diagnostic", || {
        let _permit =
            crate::storage::guard_diagnostic_operation(root, &[root.join(DIAGNOSTIC_ROOT)])?;
        if diagnostic_cwd(root, &request.cwd)? != head {
            bail!("diagnostic source HEAD changed during admission; no cache allocated");
        }
        let registry = diagnostic_directory(root, Path::new(DIAGNOSTIC_ROOT), true)?
            .context("diagnostic registry absent")?;
        let id = ulid::Ulid::new().to_string();
        let dir = registry.join(&id);
        fs::create_dir(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("lease.lock"))?;
        let mut lock = RwLock::new(lock_file);
        let _guard = lock.try_write()?;
        let target = dir.join("target");
        fs::create_dir(&target)?;
        let metadata = target.metadata()?;
        let executable = fs::canonicalize(&request.executable)?;
        let mut record = DiagnosticRecord {
            schema_version: 1,
            id,
            root: root.into(),
            cwd: request.cwd.clone(),
            head,
            executable: request.executable.clone(),
            executable_sha256: diagnostic_hash(&executable)?.sha256,
            args: request.args.clone(),
            purpose: request.purpose.clone(),
            device: metadata.dev(),
            inode: metadata.ino(),
            phase: "registered".into(),
            pid: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            removal_inferred_from_absence: false,
        };
        diagnostic_save(&dir, &record)?;
        File::open(&registry)?.sync_all()?;
        if keep {
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(dir.join(".keep"))?;
            marker.write_all(b"explicit debug retention\n")?;
            marker.sync_all()?;
        }
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("stdout.log"))?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("stderr.log"))?;
        File::open(&dir)?.sync_all()?;
        eprintln!(
            "registered diagnostic {}: target={} evidence={}",
            record.id,
            target.display(),
            dir.display()
        );
        let mut command = Command::new(&request.executable);
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            .env("CARGO_TARGET_DIR", &target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::from(stderr))
            .process_group(0);
        let mut child = command
            .spawn()
            .context("diagnostic spawn failed; registration retained")?;
        drop(command);
        record.pid = Some(child.id());
        record.phase = "running".into();
        let running_saved = diagnostic_save(&dir, &record);
        let status = child
            .wait()
            .context("diagnostic wait unknown; cache retained")?;
        record.exit_code = status.code();
        if let Err(error) = running_saved {
            return Ok(diagnostic_view(
                &dir,
                Some(&record),
                "failed",
                format!("child exited but running receipt failed: {error:#}"),
            ));
        }
        record.phase = if status.code().is_some() {
            "exited"
        } else {
            "interrupted"
        }
        .into();
        if let Err(error) = diagnostic_save(&dir, &record) {
            return Ok(diagnostic_view(
                &dir,
                Some(&record),
                "failed",
                format!("child exited but terminal receipt failed: {error:#}"),
            ));
        }
        match diagnostic_reclaim(root, &dir, &mut record, &DiagnosticKernelProbe) {
            Ok(view) => Ok(view),
            Err(error) => Ok(diagnostic_view(
                &dir,
                Some(&record),
                "failed",
                format!("cleanup retained: {error:#}"),
            )),
        }
    })
}

fn diagnostic_outside_inventory(root: &Path, views: &mut Vec<DiagnosticCacheView>) -> Result<()> {
    for relative in ["orch/target", ".worktrees", ".cowork-temp"] {
        let parent = match diagnostic_directory(root, Path::new(relative), false) {
            Ok(Some(parent)) => parent,
            Ok(None) => continue,
            Err(error) => {
                views.push(DiagnosticCacheView {
                    id: format!("outside:{relative}"),
                    path: root.join(relative),
                    disposition: "held".into(),
                    reason: format!("unsafe inventory root: {error:#}"),
                    logical_bytes: None,
                    removed_logical_bytes: 0,
                    eligible: false,
                    command_exit: None,
                    evidence_dir: root.join(relative),
                });
                continue;
            }
        };
        for entry in fs::read_dir(parent)? {
            let path = entry?.path();
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() && !meta.file_type().is_symlink() {
                continue;
            }
            views.push(DiagnosticCacheView {
                id:format!("outside:{}", path.strip_prefix(root)?.display()),
                logical_bytes:if meta.file_type().is_symlink() {None} else {apparent_tree_bytes(&path).ok()},
                path:path.clone(), disposition:"held".into(),
                reason:"outside diagnostic registry; shared/site ownership is not inferred and no adoption is performed".into(),
                removed_logical_bytes:0, eligible:false, command_exit:None, evidence_dir:path,
            });
        }
    }
    Ok(())
}

/// Inspect managed diagnostic records without creating directories, locks, or receipts.
/// Legacy/shared roots are measured and held without inferring site ownership or adopting them.
pub(crate) fn diagnostic_cache_records(root: &Path) -> Result<Vec<DiagnosticCacheView>> {
    diagnostic_root(root)?;
    let registry = diagnostic_directory(root, Path::new(DIAGNOSTIC_ROOT), false)?;
    let mut views = Vec::new();
    if let Some(registry) = registry {
        for entry in fs::read_dir(registry)? {
            let dir = entry?.path();
            if fs::symlink_metadata(&dir)?.file_type().is_symlink() || !dir.is_dir() {
                views.push(diagnostic_view(
                    &dir,
                    None,
                    "held",
                    "unregistered/unsafe registry entry".into(),
                ));
                continue;
            }
            let record = match diagnostic_read(&dir) {
                Ok(record) => record,
                Err(error) => {
                    views.push(diagnostic_view(
                        &dir,
                        None,
                        "failed",
                        format!("invalid registration: {error:#}"),
                    ));
                    continue;
                }
            };
            if let Err(error) = diagnostic_registration(root, &dir, &record) {
                views.push(diagnostic_view(
                    &dir,
                    Some(&record),
                    "failed",
                    format!("{error:#}"),
                ));
                continue;
            }
            if record.phase == "removed" {
                let absent = ["target", "retiring"].iter().all(|p| matches!(fs::symlink_metadata(dir.join(p)), Err(e) if e.kind()==io::ErrorKind::NotFound));
                views.push(diagnostic_view(&dir, Some(&record), if absent {"removed"} else {"held"},
                if absent && record.removal_inferred_from_absence {"absence observed after removing receipt; original remover and bytes unknown; no new deletion"} else if absent {"previously removed; no new deletion"} else {"path reappeared; retained"}.into()));
                continue;
            }
            let mut view = diagnostic_view(&dir, Some(&record), "held", "not eligible".into());
            let eligibility = match diagnostic_absence_receipt_ready(&dir, &record) {
                Ok(true) => {
                    view.eligible = true;
                    view.reason =
                        "eligible absence-receipt recovery; no new deletion or inferred byte count"
                            .into();
                    views.push(view);
                    continue;
                }
                Ok(false) => diagnostic_eligible(root, &dir, &record, &DiagnosticKernelProbe),
                Err(error) => Err(error),
            };
            match eligibility {
                Ok(_) => {
                    view.eligible = true;
                    view.reason = "eligible snapshot; apply rechecks all conditions".into();
                }
                Err(error) => view.reason = format!("{error:#}"),
            }
            views.push(view);
        }
    }
    views.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(views)
}

/// Inspect all diagnostic records and unregistered/shared directories without writing or adopting them.
/// No round, IR, protocol lock, or ledger repair is needed for this observation.
pub fn diagnostic_cache_status(root: &Path) -> Result<Vec<DiagnosticCacheView>> {
    let mut views = diagnostic_cache_records(root)?;
    diagnostic_outside_inventory(root, &mut views)?;
    views.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(views)
}

/// Retry only recorded diagnostics with durable child-exit evidence; never infer exit from PID loss.
/// Busy, unknown, changed, shared and explicitly kept objects remain held. No signals are sent.
pub fn sweep_managed_diagnostics(root: &Path) -> Result<Vec<DiagnosticCacheView>> {
    diagnostic_root(root)?;
    crate::reclaim::validate_storage_root(root)?;
    diagnostic_directory(root, Path::new(DIAGNOSTIC_ROOT), false)?;
    crate::close::with_protocol_ledger_effect(root, "diagnostic cache sweep", || {
        let Some(registry) = diagnostic_directory(root, Path::new(DIAGNOSTIC_ROOT), false)? else {
            return Ok(Vec::new());
        };
        let mut views = Vec::new();
        for entry in fs::read_dir(registry)? {
            let dir = entry?.path();
            if fs::symlink_metadata(&dir)?.file_type().is_symlink() || !dir.is_dir() {
                views.push(diagnostic_view(
                    &dir,
                    None,
                    "held",
                    "unsafe/unregistered registry entry".into(),
                ));
                continue;
            }
            let file = match diagnostic_file(&dir.join("lease.lock")) {
                Ok(file) => file,
                Err(error) => {
                    views.push(diagnostic_view(
                        &dir,
                        None,
                        "held",
                        format!("no trusted lock: {error:#}"),
                    ));
                    continue;
                }
            };
            let mut lock = RwLock::new(file);
            let _guard = match lock.try_write() {
                Ok(guard) => guard,
                Err(error) => {
                    views.push(diagnostic_view(
                        &dir,
                        None,
                        "held",
                        format!("diagnostic busy or lock unknown: {error}"),
                    ));
                    continue;
                }
            };
            let result = diagnostic_read(&dir).and_then(|mut record| {
                diagnostic_reclaim(root, &dir, &mut record, &DiagnosticKernelProbe)
            });
            views.push(match result {
                Ok(view) => view,
                Err(error) => diagnostic_view(&dir, None, "failed", format!("{error:#}")),
            });
        }
        Ok(views)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    fn diagnostic_fixture(tag: &str, ending: &str) -> (PathBuf, DiagnosticRequest) {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::util::test_scratch_dir(tag);
        fs::create_dir_all(root.join("orch/src")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rDiag")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(
            root.join(".gitignore"),
            "coordination/runtime/\norch/target/\n.cowork-temp/\n.worktrees/\n",
        )
        .unwrap();
        fs::write(
            root.join("orch/Cargo.toml"),
            "[package]\nname=\"diagnostic-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("orch/src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        let cargo = root.join("cargo");
        fs::write(&cargo, format!("#!/bin/sh\nset -eu\ntest -s \"${{CARGO_TARGET_DIR%/target}}/record.json\"\nprintf cache > \"$CARGO_TARGET_DIR/item\"\nprintf 'captured stdout\\n'\nprintf 'captured stderr\\n' >&2\n{ending}\n")).unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join("coordination/PROJECT-BINDING.yaml");
        let mut binding: serde_yaml::Value =
            serde_yaml::from_slice(&fs::read(source).unwrap()).unwrap();
        binding["commands"]["seedTargets"]["argv"][0] =
            serde_yaml::Value::String(cargo.to_string_lossy().into());
        fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            serde_yaml::to_string(&binding).unwrap(),
        )
        .unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rDiag\n").unwrap();
        fs::write(root.join("coordination/rounds/rDiag/events.jsonl"), "{\"eventId\":\"diag-open\",\"ts\":\"2026-09-08T00:00:00Z\",\"actor\":\"runtime:orch\",\"type\":\"RoundOpened\",\"round\":\"rDiag\",\"payload\":{\"contractSchemaVersion\":3}}\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec!["commit", "-qm", "fixture"],
        ] {
            let result = Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_COUNT", "0")
                .env("GIT_AUTHOR_NAME", "fixture")
                .env("GIT_COMMITTER_NAME", "fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let request = DiagnosticRequest {
            cwd: root.clone(),
            executable: cargo,
            args: vec![
                "test".into(),
                "--locked".into(),
                "--manifest-path".into(),
                "orch/Cargo.toml".into(),
            ],
            purpose: "bounded fixture".into(),
        };
        (root, request)
    }

    struct DiagnosticQuiet;
    impl DiagnosticProbe for DiagnosticQuiet {
        fn quiet(&self, _: &Path, _: &Path, _: u32) -> Result<()> {
            Ok(())
        }
    }
    struct DiagnosticBusy;
    impl DiagnosticProbe for DiagnosticBusy {
        fn quiet(&self, _: &Path, _: &Path, _: u32) -> Result<()> {
            bail!("group still alive or reused")
        }
    }

    fn diagnostic_kept(tag: &str) -> (PathBuf, PathBuf, DiagnosticRecord) {
        let (root, request) = diagnostic_fixture(tag, "exit 0");
        let view = run_managed_diagnostic_with_retention(&root, &request, true).unwrap();
        assert_eq!(view.command_exit, Some(0));
        assert_eq!(view.disposition, "held", "{}", view.reason);
        let record = diagnostic_read(&view.evidence_dir).unwrap();
        assert_eq!(record.phase, "terminal", "{}", view.reason);
        assert!(record.stdout.is_some() && record.stderr.is_some());
        fs::remove_file(view.evidence_dir.join(".keep")).unwrap();
        (root, view.evidence_dir, record)
    }

    #[test]
    fn maintenance_unknown_current_ledger_does_not_block_owned_diagnostic_retry() {
        let (root, dir, _) = diagnostic_kept("maintenance-independent-diagnostic");
        fs::write(
            root.join(".git/info/exclude"),
            "coordination/rounds/rBad/\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rBad")).unwrap();
        let bad = root.join("coordination/rounds/rBad/events.jsonl");
        fs::write(&bad, b"not valid JSON\n").unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rBad\n").unwrap();
        let bytes = fs::read(&bad).unwrap();
        let report = crate::reclaim::maintain_storage(&root, false).unwrap();
        assert!(!dir.join("target").exists(), "{:?}", report.items);
        assert!(report.items.iter().any(|i| i.kind == "diagnostic"
            && i.disposition == "removed"
            && i.removed_logical_bytes == 5));
        assert!(report
            .items
            .iter()
            .any(|i| i.kind == "round" && i.disposition == "failed"));
        assert_eq!(fs::read(bad).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_diagnostic_admission_uses_cache_volume_not_root_volume() {
        use crate::storage::ProbeResult;
        let (root, request) = diagnostic_fixture("maintenance-cache-volume", "exit 0");
        let entries = BTreeMap::from([
            (
                root.clone(),
                (
                    0x501,
                    ProbeResult::Ok {
                        available_bytes: 512 * 1024 * 1024 * 1024,
                        total_bytes: 1024 * 1024 * 1024 * 1024,
                    },
                ),
            ),
            (
                root.join(DIAGNOSTIC_ROOT),
                (
                    0x502,
                    ProbeResult::Ok {
                        available_bytes: 0,
                        total_bytes: 1024 * 1024 * 1024,
                    },
                ),
            ),
        ]);
        crate::storage::with_test_filesystems(&root, entries, || {
            let error = run_managed_diagnostic(&root, &request).unwrap_err();
            assert!(
                error.to_string().contains("storage admission refused"),
                "{error:#}"
            );
            let measurements = crate::storage::observe_filesystem_space(&[
                root.clone(),
                root.join(DIAGNOSTIC_ROOT),
            ]);
            assert_eq!(measurements.len(), 2);
            assert!(measurements
                .iter()
                .any(|m| m.device == 0x502 && m.available_bytes == Some(0)));
        });
        assert!(!root.join(DIAGNOSTIC_ROOT).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_diagnostic_run_is_independent_of_closed_round_state() {
        let (root, request) = diagnostic_fixture("maintenance-closed-diagnostic", "exit 0");
        let ledger = root.join("coordination/rounds/rDiag/events.jsonl");
        let mut bytes = fs::read(&ledger).unwrap();
        bytes.extend_from_slice(b"{\"eventId\":\"closed\",\"ts\":\"2026-09-08T01:00:00Z\",\"actor\":\"runtime:orch\",\"type\":\"RoundClosed\",\"round\":\"rDiag\",\"payload\":{\"forced\":false}}\n");
        fs::write(&ledger, &bytes).unwrap();
        assert!(Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-am",
                "close fixture"
            ])
            .current_dir(&root)
            .output()
            .unwrap()
            .status
            .success());
        let view = run_managed_diagnostic(&root, &request).unwrap();
        assert_eq!(view.disposition, "removed", "{}", view.reason);
        assert_eq!(fs::read(ledger).unwrap(), bytes);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_runtime_symlink_is_refused_before_any_lock_creation() {
        let (root, request) = diagnostic_fixture("maintenance-runtime-link", "exit 0");
        let victim = crate::util::test_scratch_dir("maintenance-outside-victim");
        fs::rename(
            root.join("coordination/runtime"),
            root.join("coordination/saved-runtime"),
        )
        .unwrap();
        // Keep this intentional fixture edit out of source-cleanliness checks.
        fs::write(
            root.join(".git/info/exclude"),
            "coordination/saved-runtime/\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&victim, root.join("coordination/runtime")).unwrap();
        assert!(run_managed_diagnostic(&root, &request).is_err());
        assert!(sweep_managed_diagnostics(&root).is_err());
        assert_eq!(fs::read_dir(&victim).unwrap().count(), 0);
        fs::remove_file(root.join("coordination/runtime")).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(victim).unwrap();
    }

    #[test]
    fn diagnostic_real_local_exit_preserves_logs_and_reclaims_once() {
        for code in [0, 17] {
            let (root, request) = diagnostic_fixture("diagnostic-exit", &format!("exit {code}"));
            let before = diagnostic_git(&root, &["rev-parse", "HEAD"]).unwrap();
            let view = run_managed_diagnostic(&root, &request).unwrap();
            assert_eq!(view.command_exit, Some(code));
            assert_eq!(view.disposition, "removed", "{}", view.reason);
            assert_eq!(view.removed_logical_bytes, 5);
            assert!(!view.path.exists());
            assert_eq!(
                fs::read(view.evidence_dir.join("stdout.log")).unwrap(),
                b"captured stdout\n"
            );
            assert_eq!(
                fs::read(view.evidence_dir.join("stderr.log")).unwrap(),
                b"captured stderr\n"
            );
            let again = sweep_managed_diagnostics(&root).unwrap();
            assert_eq!(again[0].removed_logical_bytes, 0);
            assert_eq!(again[0].disposition, "removed");
            assert_eq!(
                diagnostic_git(&root, &["rev-parse", "HEAD"]).unwrap(),
                before
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn diagnostic_no_exit_busy_dirty_and_changed_evidence_hold_cache() {
        let (root, dir, mut record) = diagnostic_kept("diagnostic-refusals");
        let original = record.clone();
        record.phase = "running".into();
        record.exit_code = None;
        record.pid = Some(u32::MAX);
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        record = original.clone();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticBusy)
                .unwrap()
                .disposition,
            "held"
        );
        fs::write(root.join("dirty"), b"user change").unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        fs::remove_file(root.join("dirty")).unwrap();
        fs::write(dir.join("stdout.log"), b"changed evidence").unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        assert_eq!(fs::read(dir.join("target/item")).unwrap(), b"cache");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_directory_replacement_and_symlink_never_delete_replacements() {
        use std::os::unix::fs::symlink;
        let (root, dir, mut record) = diagnostic_kept("diagnostic-aba");
        fs::rename(dir.join("target"), dir.join("preserved-original")).unwrap();
        fs::create_dir(dir.join("target")).unwrap();
        fs::write(dir.join("target/user"), b"new owner").unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        assert_eq!(fs::read(dir.join("target/user")).unwrap(), b"new owner");
        fs::remove_dir_all(dir.join("target")).unwrap();
        let victim = crate::util::test_scratch_dir("diagnostic-victim");
        fs::write(victim.join("user"), b"outside").unwrap();
        symlink(&victim, dir.join("target")).unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        assert_eq!(fs::read(victim.join("user")).unwrap(), b"outside");
        fs::remove_file(dir.join("target")).unwrap();
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(victim).unwrap();
    }

    #[test]
    fn diagnostic_interrupted_removal_retries_without_counting_or_adopting_twice() {
        let (root, dir, mut record) = diagnostic_kept("diagnostic-removing");
        record.phase = "removing".into();
        diagnostic_save(&dir, &record).unwrap();
        fs::rename(dir.join("target"), dir.join("retiring")).unwrap();
        let first = diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet).unwrap();
        assert_eq!(first.disposition, "removed");
        assert_eq!(first.removed_logical_bytes, 5);
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .removed_logical_bytes,
            0
        );
        fs::create_dir(dir.join("target")).unwrap();
        fs::write(dir.join("target/new"), b"new owner").unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        assert!(dir.join("target/new").exists());
        record.root = PathBuf::from("/not-this-repository");
        assert!(diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_missing_target_recovers_only_the_completed_removal_receipt() {
        let (root, dir, mut record) = diagnostic_kept("diagnostic-missing-receipt");
        record.phase = "removing".into();
        diagnostic_save(&dir, &record).unwrap();
        fs::remove_dir_all(dir.join("target")).unwrap();
        let stdout = fs::read(dir.join("stdout.log")).unwrap();
        fs::write(dir.join("stdout.log"), b"corrupted after removal").unwrap();
        let refused = diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet);
        assert!(refused.is_err() || refused.unwrap().disposition == "held");
        assert_eq!(diagnostic_read(&dir).unwrap().phase, "removing");
        fs::write(dir.join("stdout.log"), stdout).unwrap();
        let recovered = diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
            .expect("stable completed deletion must recover its final receipt");
        assert_eq!(recovered.disposition, "removed");
        assert_eq!(recovered.removed_logical_bytes, 0);
        assert_eq!(diagnostic_read(&dir).unwrap().phase, "removed");
        assert!(diagnostic_read(&dir).unwrap().removal_inferred_from_absence);
        assert!(recovered
            .reason
            .contains("original remover and bytes unknown"));
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .removed_logical_bytes,
            0
        );
        std::os::unix::fs::symlink(dir.join("missing-victim"), dir.join("target")).unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        fs::remove_file(dir.join("target")).unwrap();
        fs::create_dir(dir.join("target")).unwrap();
        fs::write(dir.join("target/new-owner"), b"preserve").unwrap();
        assert_eq!(
            diagnostic_reclaim(&root, &dir, &mut record, &DiagnosticQuiet)
                .unwrap()
                .disposition,
            "held"
        );
        assert_eq!(fs::read(dir.join("target/new-owner")).unwrap(), b"preserve");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_running_owner_blocks_sweep_until_actual_wait_finishes() {
        let (root, request) = diagnostic_fixture("diagnostic-concurrent", "/bin/sleep 1\nexit 0");
        let owned = root.clone();
        let running = std::thread::spawn(move || run_managed_diagnostic(&owned, &request));
        let deadline = Instant::now() + Duration::from_secs(60);
        let dir = loop {
            if let Ok(entries) = fs::read_dir(root.join(DIAGNOSTIC_ROOT)) {
                if let Some(dir) = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .find(|p| diagnostic_read(p).is_ok_and(|r| r.phase == "running"))
                {
                    break dir;
                }
            }
            assert!(Instant::now() < deadline, "running registration missing");
            std::thread::sleep(Duration::from_millis(10));
        };
        let views = sweep_managed_diagnostics(&root).unwrap();
        assert!(views
            .iter()
            .any(|v| v.evidence_dir == dir && v.disposition == "held"));
        assert!(dir.join("target").exists());
        let result = running.join().unwrap().unwrap();
        assert_eq!(result.disposition, "removed", "{}", result.reason);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_request_rejections_do_not_allocate_a_registry() {
        let (root, request) = diagnostic_fixture("diagnostic-invalid", "exit 0");
        for extra in [
            "--target-dir=/tmp/not-owned",
            "--config=build.target-dir=outside",
            "--fix",
        ] {
            let mut bad = request.clone();
            bad.args.push(extra.into());
            assert!(run_managed_diagnostic(&root, &bad).is_err());
        }
        let mut bad = request.clone();
        bad.cwd = root.parent().unwrap().to_path_buf();
        assert!(run_managed_diagnostic(&root, &bad).is_err());
        let mut bad = request.clone();
        bad.purpose = "\n".into();
        assert!(run_managed_diagnostic(&root, &bad).is_err());
        let mut bad = request;
        bad.executable = PathBuf::from("/bin/sh");
        assert!(run_managed_diagnostic(&root, &bad).is_err());
        assert!(!root.join(DIAGNOSTIC_ROOT).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_real_entry_refuses_low_space_without_task_ledger_events() {
        let (root, request) = diagnostic_fixture("diagnostic-low-space", "exit 0");
        fs::create_dir_all(root.join(".orch")).unwrap();
        // Machine settings are ignored to keep the source snapshot clean.
        fs::write(root.join(".git/info/exclude"), ".orch/\n").unwrap();
        fs::write(
            root.join(".orch/machine.yaml"),
            format!("storage:\n  floorBytes: {}\n", u64::MAX),
        )
        .unwrap();
        let ledger = root.join("coordination/rounds/rDiag/events.jsonl");
        let before = fs::read(&ledger).unwrap();
        let error = run_managed_diagnostic(&root, &request).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("diagnostic storage admission refused"),
            "{error:#}"
        );
        assert!(!root.join(DIAGNOSTIC_ROOT).exists());
        assert_eq!(fs::read(&ledger).unwrap(), before);
        assert!(crate::storage::guard_diagnostic_operation(&root, &[]).is_err());
        assert_eq!(fs::read(&ledger).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_status_is_read_only_and_does_not_adopt_legacy_roots() {
        let (root, _) = diagnostic_fixture("diagnostic-status", "exit 0");
        let legacy = root.join(".cowork-temp/legacy/target");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("sentinel"), b"unregistered").unwrap();
        let ledger = root.join("coordination/rounds/rDiag/events.jsonl");
        let before = fs::read(&ledger).unwrap();
        let index_before = fs::read(root.join(".git/index")).unwrap();
        let views = diagnostic_cache_status(&root).unwrap();
        assert!(views
            .iter()
            .any(|v| v.path == root.join(".cowork-temp/legacy")
                && v.disposition == "held"
                && !v.eligible));
        assert!(!root.join(DIAGNOSTIC_ROOT).exists());
        assert_eq!(fs::read(ledger).unwrap(), before);
        assert_eq!(fs::read(root.join(".git/index")).unwrap(), index_before);
        assert_eq!(fs::read(legacy.join("sentinel")).unwrap(), b"unregistered");
        fs::remove_dir_all(root).unwrap();
    }

    static BENCH_SEQ: AtomicU64 = AtomicU64::new(0);

    fn identity(root: &str, lock: &str) -> BuildIdentity {
        BuildIdentity {
            schema: BUILD_IDENTITY_SCHEMA,
            kind: SlotKind::TrialStaging,
            canonical_source_root: root.into(),
            canonical_common_dir: format!("{root}/.git").into(),
            rustc_version: "rustc 1.99.0".into(),
            cargo_version: "cargo 1.99.0".into(),
            target_triple: "aarch64-apple-darwin".into(),
            cargo_lock_sha256: lock.into(),
            build_config_digest: "cfg-a".into(),
        }
    }

    fn slot(tag: &str) -> TrialSlot {
        let root = crate::util::test_scratch_dir(tag);
        TrialSlot {
            index: 0,
            source_root: root.join("source"),
            root,
        }
    }

    fn malformed_target_lease() -> EventRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": "EV-BAD-LEASE",
            "ts": "2026-08-07T00:00:00Z",
            "actor": "runtime:orch",
            "type": "WorkspaceLeased",
            "round": "r66",
            "taskId": "B237",
            "payload": {
                "siteId": "B237-primary-bad agent-g01",
                "generation": 1,
                "attemptId": "B237-A0001",
                "role": "primary",
                "agent": "bad agent",
                "reviewedHead": "0123456789abcdef0123456789abcdef01234567",
                "paths": {
                    "worktree": ".worktrees/review-B237-A0001-primary-bad-agent-g01",
                    "target": "orch/target/review-B237-A0001-primary-bad-agent-g01"
                }
            }
        }))
        .unwrap()
    }

    fn implement_lease() -> EventRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": "EV-IMPLEMENT-LEASE",
            "ts": "2026-08-07T00:00:00Z",
            "actor": "runtime:orch",
            "type": "WorkspaceLeased",
            "round": "r66",
            "taskId": "B237",
            "payload": {
                "siteId": "B237-implement-executor-desktop-g01",
                "generation": 1,
                "attemptId": "B237-A0001",
                "role": "implement",
                "agent": "executor-desktop",
                "reviewedHead": "0123456789abcdef0123456789abcdef01234567",
                "wakeId": null,
                "paths": {
                    "worktree": ".worktrees/B237",
                    "target": ".worktrees/B237/orch/target"
                }
            }
        }))
        .unwrap()
    }

    fn released_review_events() -> Vec<EventRecord> {
        [
            serde_json::json!({
                "eventId": "EV-REVIEW-LEASE",
                "ts": "2026-08-07T00:00:00Z",
                "actor": "runtime:orch",
                "type": "WorkspaceLeased",
                "round": "r66",
                "taskId": "B237",
                "payload": {
                    "siteId": "B237-primary-executor-desktop-g01",
                    "generation": 1,
                    "attemptId": "B237-A0001",
                    "role": "primary",
                    "agent": "executor-desktop",
                    "reviewedHead": "0123456789abcdef0123456789abcdef01234567",
                    "paths": {
                        "worktree": ".worktrees/review-B237-A0001-primary-executor-desktop-g01",
                        "target": "orch/target/review-B237-A0001-primary-executor-desktop-g01"
                    }
                }
            }),
            serde_json::json!({
                "eventId": "EV-REVIEW-RELEASE",
                "ts": "2026-08-07T00:01:00Z",
                "actor": "runtime:orch",
                "type": "WorkspaceReleased",
                "round": "r66",
                "taskId": "B237",
                "payload": {
                    "siteId": "B237-primary-executor-desktop-g01",
                    "generation": 1,
                    "attemptId": "B237-A0001",
                    "role": "primary",
                    "agent": "executor-desktop",
                    "completionReceipt": "runtime:orch/managed-wake-terminated"
                }
            }),
        ]
        .into_iter()
        .map(|value| serde_json::from_value(value).unwrap())
        .collect()
    }

    fn events_with_role(role: &str) -> Vec<EventRecord> {
        events_for_role_agent(role, "executor-desktop")
    }

    fn events_for_role_agent(role: &str, agent: &str) -> Vec<EventRecord> {
        let mut events = released_review_events();
        for event in &mut events {
            let payload = event.payload.as_mut().unwrap();
            payload["role"] = serde_json::json!(role);
            payload["agent"] = serde_json::json!(agent);
            payload["siteId"] = serde_json::json!(format!("B237-{role}-{agent}-g01"));
            if payload.get("paths").is_some() {
                let (worktree, target) = if role == "implement" {
                    (
                        ".worktrees/B237".to_string(),
                        ".worktrees/B237/orch/target".to_string(),
                    )
                } else {
                    let name = format!("review-B237-A0001-{role}-{agent}-g01");
                    (format!(".worktrees/{name}"), format!("orch/target/{name}"))
                };
                payload["paths"] = serde_json::json!({"worktree": worktree, "target": target});
            }
        }
        events
    }

    #[test]
    fn review_role_mixed_terminal_and_active_targets_keep_governance() {
        let root = crate::util::test_scratch_dir("b332-mixed-review");
        let mut events = events_with_role("review");
        let released = "orch/target/review-B237-A0001-review-executor-desktop-g01";
        let active = "orch/target/review-B237-A0001-review-live-g01";
        let mut live = events_for_role_agent("review", "live").remove(0);
        live.event_id = "EV-ACTIVE-LEASE".into();
        events.push(live);
        for path in [released, active] {
            fs::create_dir_all(root.join(path)).unwrap();
            fs::write(root.join(path).join("sentinel"), b"cache bytes").unwrap();
        }
        let live_worktree = ".worktrees/review-B237-A0001-review-live-g01";
        fs::create_dir_all(root.join(live_worktree)).unwrap();
        fs::write(root.join(live_worktree).join("source"), b"keep source").unwrap();
        let report = sweep_targets(&root, &events, Duration::ZERO).unwrap();
        assert_eq!(report.removed, vec![released.to_string()]);
        assert!(!root.join(released).exists());
        assert!(root.join(active).join("sentinel").exists());
        assert_eq!(
            report.active_worktrees,
            vec![".worktrees/review-B237-A0001-review-live-g01"]
        );
        assert_eq!(report.active_preserved, vec![active]);
        let survey = crate::reclaim::survey_disk(&root, &Default::default(), &report).unwrap();
        let entry = survey.iter().find(|entry| entry.path == active).unwrap();
        assert_eq!(
            entry.governance,
            crate::reclaim::Governance::GovernedResidual
        );
        let worktree_entry = survey
            .iter()
            .find(|entry| entry.path == live_worktree)
            .unwrap();
        assert_eq!(
            worktree_entry.governance,
            crate::reclaim::Governance::GovernedResidual
        );
        assert_eq!(
            fs::read(root.join(live_worktree).join("source")).unwrap(),
            b"keep source"
        );
        assert!(worktree_entry.bytes > 0);
        assert!(sweep_targets(&root, &events, Duration::ZERO)
            .unwrap()
            .removed
            .is_empty());
    }

    #[test]
    fn all_site_roles_share_cache_identity_validation() {
        for role in ["primary", "secondary", "nongate", "review", "implement"] {
            let events = events_with_role(role);
            let (id, lease) = parse_target_lease(0, &events[0]).unwrap();
            assert_eq!(lease.role, role);
            assert_eq!(id, format!("B237-{role}-executor-desktop-g01"));
        }
    }

    #[test]
    fn invalid_role_type_value_missing_and_identity_refuse_before_deletion() {
        let root = crate::util::test_scratch_dir("b332-invalid-review");
        let target = root.join("orch/target/review-B237-A0001-review-executor-desktop-g01");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("sentinel"), b"preserve").unwrap();
        for (value, expected) in [
            (Some(serde_json::json!("alien")), "invalid role:"),
            (Some(serde_json::json!("Review")), "invalid role:"),
            (Some(serde_json::json!("")), "invalid role:"),
            (Some(serde_json::json!(42)), "invalid role type"),
            (None, "missing role"),
        ] {
            let mut events = events_with_role("review");
            let payload = events[0].payload.as_mut().unwrap().as_object_mut().unwrap();
            match value {
                Some(value) => {
                    payload.insert("role".into(), value);
                }
                None => {
                    payload.remove("role");
                }
            }
            let error = sweep_targets(&root, &events, Duration::ZERO).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
            assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"preserve");
        }
        let mut events = events_with_role("review");
        events[0].payload.as_mut().unwrap()["siteId"] = serde_json::json!("B237-review-other-g01");
        assert!(sweep_targets(&root, &events, Duration::ZERO)
            .unwrap_err()
            .to_string()
            .contains("siteId does not match"));
        assert!(target.join("sentinel").exists());
    }

    #[test]
    fn canonical_release_removes_the_now_inactive_target() {
        let root = crate::util::test_scratch_dir("buildcache-released-review-target");
        let target = root.join("orch/target/review-B237-A0001-primary-executor-desktop-g01");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("artifact"), b"released").unwrap();

        let report = sweep_targets(&root, &released_review_events(), Duration::ZERO).unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(!target.exists());
    }

    #[test]
    fn unique_managed_termination_reclaims_a_fresh_target() {
        let root = crate::util::test_scratch_dir("buildcache-managed-termination-target");
        let target = root.join("orch/target/review-B237-A0001-primary-executor-desktop-g01");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("artifact"), b"fresh-but-terminal").unwrap();
        let mut events = released_review_events();
        events.truncate(1);
        events[0].payload.as_mut().unwrap()["wakeId"] = serde_json::json!("wake-b263");
        events.push(
            serde_json::from_value(serde_json::json!({
                "eventId": "EV-MANAGED-TERMINATION",
                "ts": "2026-08-07T00:01:00Z",
                "actor": "runtime:orch",
                "type": "ManagedWakeTerminated",
                "round": "r66",
                "taskId": "B237",
                "payload": {
                    "wakeId": "wake-b263",
                    "agent": "executor-desktop",
                    "managedScopeTerminated": true
                }
            }))
            .unwrap(),
        );

        let report = sweep_targets(&root, &events, Duration::ZERO).unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(!target.exists());
    }

    #[test]
    fn refused_site_target_is_preserved_from_the_round_close_sweep() {
        let root = crate::util::test_scratch_dir("buildcache-protected-refusal-target");
        let relative = "orch/target/review-B237-A0001-primary-executor-desktop-g01";
        let target = root.join(relative);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("KEEP"), b"dirty-site-target").unwrap();
        let protected = BTreeSet::from([relative.to_string()]);
        let events = released_review_events();
        let eligible = terminal_target_paths(&events).unwrap();

        let report =
            sweep_targets_preserving(&root, &events, Duration::ZERO, &protected, Some(&eligible))
                .unwrap();
        assert!(report.removed.is_empty());
        assert!(report.refused.iter().any(|path| path == relative));
        assert!(target.join("KEEP").is_file());
    }

    #[test]
    fn disposition_scoped_sweep_preserves_a_target_without_lease_evidence() {
        let root = crate::util::test_scratch_dir("buildcache-disposition-only-target");
        let terminal = root.join("orch/target/review-B237-A0001-primary-executor-desktop-g01");
        let unknown = root.join("orch/target/review-B999-A0001-primary-unknown-g01");
        fs::create_dir_all(&terminal).unwrap();
        fs::create_dir_all(&unknown).unwrap();
        fs::write(terminal.join("artifact"), b"terminal").unwrap();
        fs::write(unknown.join("KEEP"), b"no-lease-evidence").unwrap();
        let events = released_review_events();
        let eligible = terminal_target_paths(&events).unwrap();

        let report = sweep_targets_preserving(
            &root,
            &events,
            Duration::ZERO,
            &BTreeSet::new(),
            Some(&eligible),
        )
        .unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(!terminal.exists());
        assert!(unknown.join("KEEP").is_file());
    }

    #[test]
    fn identity_drifted_release_blocks_the_entire_target_sweep() {
        let root = crate::util::test_scratch_dir("buildcache-drifted-release-target");
        let target = root.join("orch/target/review-B237-A0001-primary-executor-desktop-g01");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("KEEP"), b"ambiguous").unwrap();
        let mut events = released_review_events();
        events[1].payload.as_mut().unwrap()["agent"] =
            serde_json::Value::String("other-agent".to_string());

        assert!(sweep_targets(&root, &events, Duration::ZERO).is_err());
        assert!(target.join("KEEP").is_file());
    }

    #[test]
    fn active_target_with_git_marker_is_refused_but_remains_governed() {
        let root = crate::util::test_scratch_dir("buildcache-active-git-target");
        let relative = "orch/target/review-B237-A0001-primary-executor-desktop-g01";
        let target = root.join(relative);
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("artifact"), b"active").unwrap();
        fs::write(target.join(".git"), b"gitdir: /nowhere\n").unwrap();
        let mut events = released_review_events();
        events.truncate(1);

        let swept = sweep_targets(&root, &events, Duration::ZERO).unwrap();
        assert!(swept.refused.iter().any(|path| path == relative));
        assert!(swept.preserved.iter().any(|path| path == relative));
        let entries = crate::reclaim::survey_disk(
            &root,
            &crate::reclaim::TaskSiteReclaimReport::default(),
            &swept,
        )
        .unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.path == relative)
                .unwrap()
                .governance,
            crate::reclaim::Governance::GovernedResidual
        );
    }

    #[test]
    fn implement_lease_governs_worktree_without_widening_target_sweep() {
        let active = active_target_paths(&[implement_lease()]).unwrap();
        assert_eq!(
            active.worktrees,
            BTreeSet::from([".worktrees/B237".to_string()])
        );
        assert!(active.targets.is_empty());
    }

    #[test]
    fn missing_target_root_still_reports_active_implement_worktree_governance() {
        let root = crate::util::test_scratch_dir("buildcache-implement-no-target-root");
        let worktree = root.join(".worktrees/B237");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join("KEEP"), b"active").unwrap();

        let swept = sweep_targets(&root, &[implement_lease()], Duration::ZERO).unwrap();
        assert!(swept.removed.is_empty());
        assert_eq!(swept.freed_bytes, 0);
        let entries = crate::reclaim::survey_disk(
            &root,
            &crate::reclaim::TaskSiteReclaimReport::default(),
            &swept,
        )
        .unwrap();
        assert_eq!(
            entries
                .iter()
                .find(|entry| entry.path == ".worktrees/B237")
                .unwrap()
                .governance,
            crate::reclaim::Governance::GovernedResidual
        );
    }

    #[test]
    fn target_sweep_rejects_malformed_lifecycle_before_deletion() {
        let root = crate::util::test_scratch_dir("buildcache-target-bad-lease");
        let target = root.join("orch/target/review-B237-A0001-primary-bad-agent-g01");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("KEEP"), b"evidence").unwrap();

        assert!(sweep_targets(&root, &[malformed_target_lease()], Duration::ZERO).is_err());
        assert!(target.join("KEEP").is_file());
    }

    #[test]
    fn missing_target_root_does_not_hide_malformed_lifecycle() {
        let root = crate::util::test_scratch_dir("buildcache-target-missing");
        assert!(sweep_targets(&root, &[malformed_target_lease()], Duration::ZERO).is_err());
        assert!(!root.join("orch/target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn target_root_symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let root = crate::util::test_scratch_dir("buildcache-target-root-symlink");
        let outside = root.join("outside/review-B237-A0001-primary-agent-g01");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("KEEP"), b"outside").unwrap();
        fs::create_dir_all(root.join("orch")).unwrap();
        symlink(root.join("outside"), root.join("orch/target")).unwrap();

        assert!(sweep_targets(&root, &[], Duration::ZERO).is_err());
        assert!(outside.join("KEEP").is_file());
        assert!(crate::reclaim::survey_disk(
            &root,
            &crate::reclaim::TaskSiteReclaimReport::default(),
            &SweepTargetsReport::default(),
        )
        .is_err());
    }

    #[test]
    fn sweep_preserves_slot_lock_source_and_non_generation_entries() {
        let repo = crate::util::test_scratch_dir("buildcache-sweep-boundaries");
        let slot = repo.join(".cowork-temp/trial-cache/slots/slot-00");
        fs::create_dir_all(slot.join("source")).unwrap();
        fs::write(slot.join("source/KEEP"), b"source").unwrap();
        fs::write(slot.join("slot.lock"), b"lock-sentinel").unwrap();
        fs::create_dir_all(slot.join("generations/not-a-generation")).unwrap();
        fs::write(slot.join("generations/not-a-generation/KEEP"), b"not-owned").unwrap();
        for generation in 1..=4 {
            let target = slot.join(format!("generations/generation-{generation:06}/target"));
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("artifact"), vec![generation as u8; 512]).unwrap();
        }

        let first = sweep_trial_cache(&repo, 2).unwrap();
        assert_eq!(first.removed, 2);
        assert!(first.freed_bytes >= 1024);
        assert_eq!(fs::read(slot.join("slot.lock")).unwrap(), b"lock-sentinel");
        assert_eq!(fs::read(slot.join("source/KEEP")).unwrap(), b"source");
        assert!(slot.join("generations/generation-000003").is_dir());
        assert!(slot.join("generations/generation-000004").is_dir());
        assert!(slot.join("generations/not-a-generation/KEEP").is_file());

        assert_eq!(
            sweep_trial_cache(&repo, 2).unwrap(),
            TrialCacheSweepReport::default()
        );
    }

    #[test]
    fn budgeted_sweep_keeps_the_newest_suffix_that_fits() {
        let repo = crate::util::test_scratch_dir("buildcache-sweep-budget-suffix");
        let slot = repo.join(".cowork-temp/trial-cache/slots/slot-00");
        for (generation, bytes) in [(1, 900usize), (2, 700), (3, 600)] {
            let target = slot.join(format!("generations/generation-{generation:06}/target"));
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("artifact"), vec![generation as u8; bytes]).unwrap();
        }

        let report = sweep_trial_cache_with_budget(&repo, 3, 1_400).unwrap();
        assert_eq!(report.removed, 1);
        assert!(report.freed_bytes >= 900);
        assert_eq!(report.slot_usage.len(), 1);
        assert!(report.slot_usage[0].logical_bytes_after <= 1_400);
        assert!(!slot.join("generations/generation-000001").exists());
        assert!(slot.join("generations/generation-000002").is_dir());
        assert!(slot.join("generations/generation-000003").is_dir());
    }

    #[test]
    fn mismatch_allocates_fresh_generation_and_preserves_old_evidence() {
        let slot = slot("buildcache-mismatch-generation");
        let first = prepare_trial_target(&slot, &identity("/root-a", "lock-a"), None).unwrap();
        let sentinel = first.target_dir().join("old-evidence");
        fs::write(&sentinel, b"preserve").unwrap();
        let marker_before = fs::read(first.target_dir().join(MARKER_FILE)).unwrap();

        let second = prepare_trial_target(&slot, &identity("/root-b", "lock-a"), None).unwrap();
        assert!(second.generation() > first.generation());
        assert_ne!(second.target_dir(), first.target_dir());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        assert_eq!(
            fs::read(first.target_dir().join(MARKER_FILE)).unwrap(),
            marker_before,
            "mismatched marker must never be rewritten"
        );
    }

    #[test]
    fn missing_or_corrupt_marker_on_nonempty_target_is_never_rebound() {
        let slot = slot("buildcache-bad-marker");
        let generations = slot.root().join("generations");
        let unmarked = generation_target(&generations, 0);
        fs::create_dir_all(&unmarked).unwrap();
        fs::write(unmarked.join("artifact"), b"old").unwrap();
        let fresh = prepare_trial_target(&slot, &identity("/root-a", "lock-a"), None).unwrap();
        assert_eq!(fresh.generation(), 1);
        assert_eq!(fs::read(unmarked.join("artifact")).unwrap(), b"old");

        fs::write(fresh.target_dir().join(MARKER_FILE), b"not-json").unwrap();
        let newer = prepare_trial_target(&slot, &identity("/root-a", "lock-a"), None).unwrap();
        assert_eq!(newer.generation(), 2);
        assert_eq!(
            fs::read(fresh.target_dir().join(MARKER_FILE)).unwrap(),
            b"not-json"
        );
    }

    #[test]
    fn warm_clone_invalidates_workspace_and_retains_dependencies() {
        let slot = slot("buildcache-warm-invalidation");
        let hot = slot.root().join("hot");
        fs::create_dir_all(hot.join("debug/.fingerprint/orch-host-aaa")).unwrap();
        fs::create_dir_all(hot.join("debug/.fingerprint/serde-bbb")).unwrap();
        fs::create_dir_all(hot.join("debug/incremental/orch_host-aaa")).unwrap();
        fs::create_dir_all(hot.join("debug/deps")).unwrap();
        fs::write(hot.join("debug/deps/liborch_host_aaa.rmeta"), b"workspace").unwrap();
        fs::write(hot.join("debug/deps/libserde_bbb.rmeta"), b"dependency").unwrap();

        let prepared = prepare_trial_target(
            &slot,
            &identity("/stable/trial/source", "lock-a"),
            Some(&hot),
        )
        .unwrap();
        assert!(matches!(
            prepared.warm_preparation(),
            WarmPreparation::Warmed { .. }
        ));
        assert!(!prepared
            .target_dir()
            .join("debug/.fingerprint/orch-host-aaa")
            .exists());
        assert!(!prepared
            .target_dir()
            .join("debug/incremental/orch_host-aaa")
            .exists());
        assert!(!prepared
            .target_dir()
            .join("debug/deps/liborch_host_aaa.rmeta")
            .exists());
        assert_eq!(
            fs::read(prepared.target_dir().join("debug/deps/libserde_bbb.rmeta")).unwrap(),
            b"dependency"
        );
    }

    #[test]
    fn warm_log_requires_workspace_rebuild_without_dependency_rebuilds() {
        let proven = assess_warm_log(
            0,
            "   Compiling orch-core v0.1.0\n   Compiling orch-host v0.1.0\n",
        );
        assert!(matches!(proven, WarmStartAssessment::Proven { .. }));
        let dependency_rebuilt = assess_warm_log(
            0,
            "Compiling serde v1.0.0\nCompiling orch-core v0.1.0\nCompiling orch-host v0.1.0\n",
        );
        assert!(dependency_rebuilt.requires_fallback());
        let workspace_not_rebuilt = assess_warm_log(0, "Finished test profile\n");
        assert!(workspace_not_rebuilt.requires_fallback());
    }

    #[test]
    fn unproven_warm_result_opens_a_disclosed_cold_generation_and_preserves_evidence() {
        let slot = slot("buildcache-warm-fallback");
        let hot = slot.root().join("hot");
        fs::create_dir_all(hot.join("debug/.fingerprint/orch-host-aaa")).unwrap();
        fs::create_dir_all(hot.join("debug/.fingerprint/serde-bbb")).unwrap();
        fs::create_dir_all(hot.join("debug/deps")).unwrap();
        fs::write(hot.join("debug/deps/liborch_host_aaa.rmeta"), b"workspace").unwrap();
        fs::write(hot.join("debug/deps/libserde_bbb.rmeta"), b"dependency").unwrap();

        let identity = identity("/stable/trial/source", "lock-a");
        let warm = prepare_trial_target(&slot, &identity, Some(&hot)).unwrap();
        assert!(matches!(
            warm.warm_preparation(),
            WarmPreparation::Warmed { .. }
        ));
        let marker_before = fs::read(warm.target_dir().join(MARKER_FILE)).unwrap();
        let sentinel = warm.target_dir().join("warm-evidence");
        fs::write(&sentinel, b"preserve").unwrap();

        let assessment = assess_warm_log(
            0,
            "Compiling serde v1.0.0\nCompiling orch-core v0.1.0\nCompiling orch-host v0.1.0\n",
        );
        assert!(assessment.requires_fallback());
        let cold = prepare_cold_fallback_target(&slot, &identity, assessment.render()).unwrap();
        assert!(cold.generation() > warm.generation());
        assert_ne!(cold.target_dir(), warm.target_dir());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        assert_eq!(
            fs::read(warm.target_dir().join(MARKER_FILE)).unwrap(),
            marker_before
        );
        let disclosure = cold.warm_preparation().render();
        assert!(disclosure.contains("trial-cache fallback: cold private generation"));
        assert!(disclosure.contains("warm gate rebuilt registry dependencies"));
        cold.verify_before_spawn().unwrap();
    }

    #[test]
    fn config_digest_binds_argv_order_and_trial_timeout() {
        let commands = BTreeMap::from([(
            "testFast".to_string(),
            CommandSpec {
                argv: vec!["cargo".into(), "test".into(), "--locked".into()],
                timeout_seconds: 7,
                trial_timeout_seconds: None,
                approval: None,
            },
        )]);
        let refs = vec!["testFast".to_string()];
        let first = build_config_digest(&refs, &commands).unwrap();
        let mut explicit = commands;
        explicit.get_mut("testFast").unwrap().trial_timeout_seconds = Some(21);
        assert_ne!(first, build_config_digest(&refs, &explicit).unwrap());
    }

    #[test]
    fn overflow_saturates_instead_of_wrapping() {
        assert_eq!(resolve_trial_timeout_secs(u64::MAX, None), u64::MAX);
        assert_eq!(resolve_trial_timeout_secs(7, None), 21);
        assert_eq!(resolve_trial_timeout_secs(7, Some(11)), 11);
    }

    struct BenchGuard {
        repo: PathBuf,
        source: PathBuf,
        root: PathBuf,
    }

    impl Drop for BenchGuard {
        fn drop(&mut self) {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .args(["worktree", "remove", "--force"])
                .arg(&self.source)
                .output();
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn benchmark_gate(cargo: &Path, source: &Path, target: &Path) -> (i32, u128, String) {
        let start = Instant::now();
        let output = Command::new(cargo)
            .args([
                "test",
                "--workspace",
                "--locked",
                "--no-run",
                "--manifest-path",
            ])
            .arg(source.join("orch/Cargo.toml"))
            .current_dir(source)
            .env("CARGO_TARGET_DIR", target)
            .output()
            .unwrap();
        let elapsed = start.elapsed().as_millis();
        let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
        log.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.code().unwrap_or(-1), elapsed, log)
    }

    struct TrialLogProbeGuard(PathBuf);

    impl Drop for TrialLogProbeGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Manual production-path proof for B232: use a reflink-warmed target, execute a real Cargo
    /// command through `run_trial_gate`, then derive the warm verdict by rereading that exact log.
    #[test]
    #[ignore = "B232 real Cargo trial-gate log acceptance probe"]
    fn real_trial_gate_log_preserves_compiling_and_drives_warm_assessment() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = manifest
            .ancestors()
            .nth(3)
            .expect("orch-host manifest must live below the outer repository")
            .to_path_buf();
        let root = source.join(".cowork-temp").join(format!(
            "b232-real-trial-log-{}-{}",
            std::process::id(),
            BENCH_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let _guard = TrialLogProbeGuard(root.clone());
        let slot = TrialSlot {
            index: 0,
            source_root: source.clone(),
            root: root.join("slot"),
        };
        let cargo = std::env::var_os("CARGO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cargo"));
        let identity = inspect_build_identity(&source, &cargo, "b232-real-trial-gate".into())
            .expect("inspect real trial identity");
        let prepared = prepare_trial_target(&slot, &identity, Some(&source.join("orch/target")))
            .expect("prepare reflink-warmed trial target");
        assert!(
            matches!(prepared.warm_preparation(), WarmPreparation::Warmed { .. }),
            "acceptance probe requires a real warm preparation: {}",
            prepared.warm_preparation().render()
        );
        let spec = CommandSpec {
            argv: vec![
                cargo.to_string_lossy().into_owned(),
                "test".into(),
                "--locked".into(),
                "--manifest-path".into(),
                "orch/Cargo.toml".into(),
                "-p".into(),
                "orch-host".into(),
                "--lib".into(),
                "gate::tests::reuse_disclosure_line_contains_key_and_log".into(),
                "--".into(),
                "--exact".into(),
            ],
            timeout_seconds: 600,
            trial_timeout_seconds: Some(600),
            approval: None,
        };
        let result = crate::gate::run_trial_gate(
            "testFast",
            &spec,
            &source,
            &root.join("logs"),
            "b232-real",
            &prepared,
        )
        .expect("run real trial gate");
        assert_eq!(result.exit_code, 0);
        let log = fs::read_to_string(&result.log_path).unwrap();
        assert!(
            log.contains("Compiling orch-core"),
            "missing orch-core compile line"
        );
        assert!(
            log.contains("Compiling orch-host"),
            "missing orch-host compile line"
        );
        assert!(log.contains("test result:"), "missing real test stdout");
        let assessment = prepared
            .assess_gate_log(result.exit_code, Path::new(&result.log_path))
            .unwrap();
        assert!(!matches!(assessment, WarmStartAssessment::NotApplicable));
        eprintln!(
            "B232_REAL_TRIAL_GATE exit={} bytes={} core=true host=true testOutput=true {}",
            result.exit_code,
            log.len(),
            assessment.render()
        );
    }

    /// Manual acceptance probe for the APFS warm path and its mandatory fallback.  It uses a
    /// detached source root and compares an empty target with a reflink-cloned target.  A proven
    /// warm log must show both workspace crates rebuilding without registry recompiles; otherwise
    /// the measured unproven result is explicitly classified as the cold-fallback branch.
    #[test]
    #[ignore = "B208 slow APFS warm-vs-cold acceptance probe"]
    fn warm_start_measurably_rebuilds_workspace_crates_and_reuses_registry_deps() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = manifest
            .ancestors()
            .nth(3)
            .expect("orch-host manifest must live below the outer repository")
            .to_path_buf();
        let root = repo.join(".cowork-temp").join(format!(
            "b208-warm-bench-{}-{}",
            std::process::id(),
            BENCH_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let add = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "--detach"])
            .arg(&source)
            .arg("HEAD")
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );
        let _guard = BenchGuard {
            repo: repo.clone(),
            source: source.clone(),
            root: root.clone(),
        };

        let hot = repo.join("orch/target");
        assert!(
            directory_nonempty(&hot),
            "hot target must be populated first"
        );
        let cold = root.join("cold-target");
        let warm = root.join("warm-target");
        fs::create_dir_all(&cold).unwrap();
        fs::create_dir_all(&warm).unwrap();
        let method = clone_hot_target(&hot, &warm).unwrap();
        assert_eq!(method, WarmCloneMethod::ApfsReflink);
        let invalidation = invalidate_workspace_artifacts(&warm).unwrap();
        assert!(invalidation.invalidated_workspace_entries > 0);
        assert!(invalidation.retained_dependency_entries > 0);

        let cargo = std::env::var_os("CARGO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cargo"));
        let (cold_exit, cold_ms, cold_log) = benchmark_gate(&cargo, &source, &cold);
        assert_eq!(cold_exit, 0, "cold gate failed:\n{cold_log}");
        let (warm_exit, warm_ms, warm_log) = benchmark_gate(&cargo, &source, &warm);
        assert_eq!(warm_exit, 0, "warm gate failed:\n{warm_log}");
        let assessment = assess_warm_log(warm_exit, &warm_log);
        match &assessment {
            WarmStartAssessment::Proven { .. } => {
                assert!(
                    warm_ms < cold_ms,
                    "warm target must be measurably faster: cold={cold_ms}ms warm={warm_ms}ms"
                );
                eprintln!(
                    "B208_WARM_BENCH cold_ms={cold_ms} warm_ms={warm_ms} {}",
                    assessment.render()
                );
            }
            WarmStartAssessment::FallbackRequired { .. } => {
                eprintln!(
                    "B208_WARM_FALLBACK cold_ms={cold_ms} warm_ms={warm_ms} {}",
                    assessment.render()
                );
            }
            WarmStartAssessment::NotApplicable => {
                panic!("a cloned warm target must yield a proof or an explicit fallback")
            }
        }
    }

    /// Directed H88 reproduction at the rmeta boundary.  Root A writes metadata without
    /// `new_symbol`; root B contains the symbol, but a consumer pointed at A's stale target still
    /// fails E0432.  Recompiling B into a fresh generation turns the identical consumer green,
    /// while the marker decision refuses A's generation before a Cargo process could use it.
    #[test]
    #[ignore = "B208 slow H88 stale-rmeta reproduction"]
    fn h88_cross_root_target_reproduces_e0432_and_marker_refuses_it() {
        let root = crate::util::test_scratch_dir("buildcache-h88-reproduction");
        let root_a = root.join("root-a");
        let root_b = root.join("root-b");
        let shared = root.join("shared-target");
        let fresh = root.join("fresh-target");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::write(root_a.join("lib.rs"), "pub fn old_symbol() {}\n").unwrap();
        fs::write(root_b.join("lib.rs"), "pub fn new_symbol() {}\n").unwrap();
        fs::write(
            root_b.join("consumer.rs"),
            "use h88_lib::new_symbol;\nfn main() { new_symbol(); }\n",
        )
        .unwrap();
        let rustc = sibling_rustc(Path::new(
            &std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()),
        ));
        let stale_artifact = shared.join("libh88_lib.rlib");
        let a = Command::new(&rustc)
            .args(["--crate-name", "h88_lib", "--crate-type", "lib"])
            .arg(root_a.join("lib.rs"))
            .arg("-o")
            .arg(&stale_artifact)
            .output()
            .unwrap();
        assert!(
            a.status.success(),
            "root A metadata build failed: {}",
            String::from_utf8_lossy(&a.stderr)
        );
        let b = Command::new(&rustc)
            .arg("--edition=2021")
            .arg(root_b.join("consumer.rs"))
            .arg("--extern")
            .arg(format!("h88_lib={}", stale_artifact.display()))
            .arg("-o")
            .arg(shared.join("consumer"))
            .output()
            .unwrap();
        let b_log = String::from_utf8_lossy(&b.stderr);
        assert!(
            !b.status.success() && b_log.contains("error[E0432]") && b_log.contains("new_symbol"),
            "H88 stale-rmeta reproduction did not produce E0432:\n{b_log}"
        );

        let fresh_artifact = fresh.join("libh88_lib.rlib");
        let rebuild = Command::new(&rustc)
            .args(["--crate-name", "h88_lib", "--crate-type", "lib"])
            .arg(root_b.join("lib.rs"))
            .arg("-o")
            .arg(&fresh_artifact)
            .output()
            .unwrap();
        assert!(rebuild.status.success());
        let green = Command::new(&rustc)
            .arg("--edition=2021")
            .arg(root_b.join("consumer.rs"))
            .arg("--extern")
            .arg(format!("h88_lib={}", fresh_artifact.display()))
            .arg("-o")
            .arg(fresh.join("consumer"))
            .output()
            .unwrap();
        assert!(
            green.status.success(),
            "fresh B generation must compile: {}",
            String::from_utf8_lossy(&green.stderr)
        );

        let identity_a = identity(&root_a.to_string_lossy(), "lock-a");
        let identity_b = identity(&root_b.to_string_lossy(), "lock-a");
        assert!(matches!(
            TrialSlot::decide(Some(&identity_a), &identity_b),
            CacheDecision::FreshGeneration { .. }
        ));
        eprintln!(
            "B208_H88_REPRO stale_exit={} error=E0432 fresh_exit={} marker=FreshGeneration",
            b.status, green.status
        );
        fs::remove_dir_all(&root).unwrap();
    }
}

// Keep this pure decision at the end of the file.  The seeded contract scans everything after the
// signature and rejects filesystem or process side effects there.
impl TrialSlot {
    pub fn decide(existing: Option<&BuildIdentity>, incoming: &BuildIdentity) -> CacheDecision {
        match existing {
            Some(existing) if existing.matches(incoming) => CacheDecision::ReuseIncremental,
            Some(existing) => CacheDecision::FreshGeneration {
                reason: existing.mismatch_reason(incoming),
            },
            None => CacheDecision::FreshGeneration {
                reason: "marker missing or unreadable identity".to_string(),
            },
        }
    }
}
