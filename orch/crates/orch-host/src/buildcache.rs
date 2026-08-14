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

use orch_core::{read_ledger, EventRecord};

use crate::binding::CommandSpec;

pub const MARKER_FILE: &str = ".orch-build-identity.json";
pub const TRIAL_SLOT_COUNT: usize = 4;
const GENERATION_PREFIX: &str = "generation-";
const BUILD_IDENTITY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrialCacheSweepReport {
    pub removed: usize,
    pub freed_bytes: u64,
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
    let role = payload_string(event, "role")
        .filter(|value| matches!(*value, "primary" | "secondary" | "nongate" | "implement"))
        .context("WorkspaceLeased missing role")?;
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

/// Sweep only direct child directories on a conservative name whitelist. Active lease targets,
/// young entries, non-directories, unknown names, and anything with a top-level `.git` are kept.
pub fn sweep_targets(
    root: &Path,
    events: &[EventRecord],
    ttl: Duration,
) -> Result<SweepTargetsReport> {
    sweep_targets_preserving(root, events, ttl, &BTreeSet::new(), None)
}

fn sweep_targets_preserving(
    root: &Path,
    events: &[EventRecord],
    ttl: Duration,
    protected_targets: &BTreeSet<String>,
    eligible_targets: Option<&BTreeSet<String>>,
) -> Result<SweepTargetsReport> {
    let target_root = root.join("orch/target");
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

    let mut removals = Vec::<(PathBuf, String, u64)>::new();
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
        if eligible_targets.is_some_and(|eligible| !eligible.contains(&relative)) {
            // Round close is disposition-scoped: a path with no canonical
            // terminal lease stays visible to the disk survey and can still
            // be handled by the explicit TTL-based maintenance command.
            report.preserved.push(relative);
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
        if !old_enough(&path, ttl) {
            report.governed_preserved.insert(relative.clone());
            report.preserved.push(relative);
            continue;
        }

        let bytes = apparent_tree_bytes(&path)?;
        removals.push((path, relative, bytes));
    }

    report.active_worktrees = active_paths.worktrees.into_iter().collect();

    // All fallible discovery and measurement completes before the first deletion. Once mutation
    // starts, failures remain visible in the report instead of discarding earlier successes.
    for (path, relative, bytes) in removals {
        let root_is_real = fs::symlink_metadata(&target_root)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            .unwrap_or(false);
        let entry_is_real = fs::symlink_metadata(&path)
            .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
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
    disposition_only: bool,
) -> Result<SweepTargetsReport> {
    crate::close::with_protocol_effect(root, "sites sweep-targets", || {
        let round = crate::current_round(root)?;
        let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let mut report = None;
        crate::ledger::append_checked(root, &round, |_| {
            // append_checked first reconciles any pending atomic ledger publication, then keeps the
            // exact ledger lock held across this fresh read, lifecycle fold, and deletion phase.
            let ledger = read_ledger(&ledger_path).with_context(|| {
                format!(
                    "read current round ledger failed: {}",
                    ledger_path.display()
                )
            })?;
            if !ledger.bad_lines.is_empty() {
                bail!("current round ledger contains bad lines");
            }
            if ledger.events.is_empty() {
                bail!("current round ledger is empty");
            }
            let ledger_bytes = fs::read(&ledger_path).with_context(|| {
                format!(
                    "read current round ledger bytes failed: {}",
                    ledger_path.display()
                )
            })?;
            let wal_path = root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl"));
            match fs::read(&wal_path) {
                Ok(wal_bytes) if wal_bytes == ledger_bytes => {}
                Ok(_) => bail!("current round ledger/WAL bytes diverge before target sweep"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("read current round WAL failed: {}", wal_path.display())
                    })
                }
            }
            let protected_targets =
                protected_target_paths_for_sites(&ledger.events, protected_site_ids)?;
            let eligible_targets = disposition_only
                .then(|| terminal_target_paths(&ledger.events))
                .transpose()?;
            report = Some(sweep_targets_preserving(
                root,
                &ledger.events,
                ttl,
                &protected_targets,
                eligible_targets.as_ref(),
            )?);
            Ok(Vec::new())
        })?;
        report.context("target sweep completed without a report")
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
/// generations.  The stable `source/` worktree and `slot.lock` live beside `generations/`, so the
/// traversal never enters or removes either of them.  Existing production slot locks are acquired
/// before deletion; fixture/legacy slots without a lock remain sweepable without creating one.
pub fn sweep_trial_cache(repo_root: &Path, keep_latest: usize) -> Result<TrialCacheSweepReport> {
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
        sweep_trial_slot(&slot, keep_latest, &mut report)?;
    }
    Ok(report)
}

fn sweep_trial_slot(
    slot: &Path,
    keep_latest: usize,
    report: &mut TrialCacheSweepReport,
) -> Result<()> {
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
            sweep_slot_generations(slot, keep_latest, report)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            sweep_slot_generations(slot, keep_latest, report)
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
    report: &mut TrialCacheSweepReport,
) -> Result<()> {
    let generations_root = slot.join("generations");
    let entries = match fs::read_dir(&generations_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
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
        generations.push((number, entry.path()));
    }
    generations.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let remove_count = generations.len().saturating_sub(keep_latest);
    for (_, generation) in generations.into_iter().take(remove_count) {
        let bytes = apparent_tree_bytes(&generation)?;
        crate::util::remove_dir_all_with_enotempty_retry(&generation).with_context(|| {
            format!(
                "remove old trial cache generation failed: {}",
                generation.display()
            )
        })?;
        report.removed = report.removed.saturating_add(1);
        report.freed_bytes = report.freed_bytes.saturating_add(bytes);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

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
    fn missing_target_root_is_default_even_with_malformed_lifecycle() {
        let root = crate::util::test_scratch_dir("buildcache-target-missing");
        let report = sweep_targets(&root, &[malformed_target_lease()], Duration::ZERO).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.freed_bytes, 0);
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
