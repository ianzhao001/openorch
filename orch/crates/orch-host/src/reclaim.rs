//! Fail-closed reclamation of completed task worktrees and disk-governance reporting.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};

use crate::gitx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Criterion {
    Met,
    Unmet,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSiteCriteria {
    pub ledger_recorded: Criterion,
    pub executor_process_absent: Criterion,
    pub worktree_clean: Criterion,
    pub head_merged_into_main: Criterion,
}

impl TaskSiteCriteria {
    pub fn fail_closed() -> Self {
        Self {
            ledger_recorded: Criterion::Indeterminate,
            executor_process_absent: Criterion::Indeterminate,
            worktree_clean: Criterion::Indeterminate,
            head_merged_into_main: Criterion::Indeterminate,
        }
    }

    pub fn with_ledger_recorded(mut self, criterion: Criterion) -> Self {
        self.ledger_recorded = criterion;
        self
    }

    pub fn with_executor_process_absent(mut self, criterion: Criterion) -> Self {
        self.executor_process_absent = criterion;
        self
    }

    pub fn with_worktree_clean(mut self, criterion: Criterion) -> Self {
        self.worktree_clean = criterion;
        self
    }

    pub fn with_head_merged_into_main(mut self, criterion: Criterion) -> Self {
        self.head_merged_into_main = criterion;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSiteDisposition {
    Reclaim,
    Keep { reason: String },
}

impl TaskSiteDisposition {
    pub fn is_reclaim(&self) -> bool {
        matches!(self, Self::Reclaim)
    }

    pub fn keep_reason(&self) -> Option<&str> {
        match self {
            Self::Reclaim => None,
            Self::Keep { reason } => Some(reason),
        }
    }
}

pub fn decide_task_site_reclaim(criteria: &TaskSiteCriteria) -> TaskSiteDisposition {
    let blockers = [
        ("ledger_recorded", criteria.ledger_recorded),
        ("executor_process_absent", criteria.executor_process_absent),
        ("worktree_clean", criteria.worktree_clean),
        ("head_merged_into_main", criteria.head_merged_into_main),
    ]
    .into_iter()
    .filter(|(_, criterion)| *criterion != Criterion::Met)
    .map(|(name, criterion)| format!("{name}={criterion:?}"))
    .collect::<Vec<_>>();

    if blockers.is_empty() {
        TaskSiteDisposition::Reclaim
    } else {
        TaskSiteDisposition::Keep {
            reason: blockers.join(", "),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RecordedTasks {
    pub by_task: BTreeMap<String, String>,
    pub skipped_rounds: Vec<String>,
}

pub(crate) fn recorded_tasks_from_events(events: &[EventRecord]) -> RecordedTasks {
    let mut recorded = RecordedTasks::default();
    for event in events {
        if event.kind != "TaskRecorded" || event.actor != "runtime:orch" {
            continue;
        }
        let (Some(task_id), Some(round)) = (event.task_id.as_deref(), event.round.as_deref())
        else {
            continue;
        };
        if task_id.is_empty() || round.is_empty() {
            continue;
        }
        match recorded.by_task.get(task_id) {
            Some(previous) if !candidate_round_is_newer(round, previous) => {}
            _ => {
                recorded
                    .by_task
                    .insert(task_id.to_string(), round.to_string());
            }
        }
    }
    recorded
}

fn round_number(round: &str) -> Option<u64> {
    round.strip_prefix('r')?.parse().ok()
}

fn candidate_round_is_newer(candidate: &str, current: &str) -> bool {
    match (round_number(candidate), round_number(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => candidate > current,
    }
}

fn task_record_is_canonical(event: &orch_core::EventRecord, round: &str) -> bool {
    event.kind != "TaskRecorded"
        || (event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event
                .task_id
                .as_deref()
                .is_some_and(|task| !task.is_empty())
            && event.extra.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "plannerWakeId" | "initiatorKind" | "invocationMode"
                )
            })
            && event.payload.as_ref() == Some(&serde_json::json!({"postMergeGates": "all-green"})))
}

/// Fold every readable, uncorrupted round ledger. A corrupt or unreadable individual round is
/// skipped in full so a valid-looking prefix can never establish a destructive fact.
pub fn tasks_recorded_in_any_round(root: &Path) -> Result<RecordedTasks> {
    let rounds_root = root.join("coordination/rounds");
    let entries = fs::read_dir(&rounds_root)
        .with_context(|| format!("read rounds directory failed: {}", rounds_root.display()))?;
    let mut recorded = RecordedTasks::default();
    let mut ledgers = Vec::<(String, PathBuf)>::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(round) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if round_number(&round).is_none() {
            continue;
        }
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => {}
            Ok(_) => continue,
            Err(_) => {
                recorded.skipped_rounds.push(round);
                continue;
            }
        }
        let ledger_path = entry.path().join("events.jsonl");
        match fs::symlink_metadata(&ledger_path) {
            Ok(metadata) if metadata.is_file() => ledgers.push((round, ledger_path)),
            Ok(_) | Err(_) => recorded.skipped_rounds.push(round),
        }
    }
    ledgers.sort_by(|left, right| left.0.cmp(&right.0));

    for (round, ledger_path) in ledgers {
        let ledger = match read_ledger(&ledger_path) {
            Ok(ledger) if ledger.bad_lines.is_empty() => ledger,
            Ok(_) | Err(_) => {
                recorded.skipped_rounds.push(round);
                continue;
            }
        };
        if !ledger
            .events
            .iter()
            .all(|event| task_record_is_canonical(event, &round))
        {
            recorded.skipped_rounds.push(round);
            continue;
        }
        for event in ledger.events {
            if event.kind != "TaskRecorded" {
                continue;
            }
            let Some(task_id) = event.task_id.filter(|task| !task.is_empty()) else {
                continue;
            };
            match recorded.by_task.get(&task_id) {
                Some(previous) if !candidate_round_is_newer(&round, previous) => {}
                _ => {
                    recorded.by_task.insert(task_id, round.clone());
                }
            }
        }
    }
    recorded.skipped_rounds.sort();
    recorded.skipped_rounds.dedup();
    Ok(recorded)
}

fn probe_executor_process_absent(root: &Path, task_id: &str) -> Criterion {
    let heartbeats = root.join("coordination/runtime/heartbeats");
    let entries = match fs::read_dir(&heartbeats) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Criterion::Met,
        Err(_) => return Criterion::Indeterminate,
    };
    let generation_prefix = format!("{task_id}-");
    let mut indeterminate = false;
    let mut live_candidate = false;

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                indeterminate = true;
                continue;
            }
        };
        if entry.path().extension() != Some(OsStr::new("json")) {
            continue;
        }
        let text = match fs::read_to_string(entry.path()) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                indeterminate = true;
                continue;
            }
        };
        let heartbeat = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(heartbeat) => heartbeat,
            Err(_) => {
                indeterminate = true;
                continue;
            }
        };
        let Some(generation) = heartbeat
            .get("generation")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        if !generation.starts_with(&generation_prefix) {
            continue;
        }
        let Some(pid) = heartbeat
            .get("pid")
            .and_then(serde_json::Value::as_i64)
            .filter(|pid| *pid > 0)
        else {
            indeterminate = true;
            continue;
        };
        match Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => live_candidate = true,
            Ok(_) => {}
            Err(_) => indeterminate = true,
        }
    }

    if live_candidate {
        Criterion::Unmet
    } else if indeterminate {
        Criterion::Indeterminate
    } else {
        Criterion::Met
    }
}

fn probe_worktree_clean(worktree: &Path) -> Criterion {
    match Command::new("git")
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v2",
        ])
        .current_dir(worktree)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
    {
        Ok(output) if output.status.success() && output.stdout.is_empty() => Criterion::Met,
        Ok(output) if output.status.success() => Criterion::Unmet,
        _ => Criterion::Indeterminate,
    }
}

fn probe_head_merged_into_main(root: &Path, worktree: &Path) -> Criterion {
    let head = match gitx::rev_parse(worktree, "HEAD") {
        Ok(head) => head,
        Err(_) => return Criterion::Indeterminate,
    };
    match gitx::is_ancestor(root, &head, "refs/heads/main") {
        Ok(true) => Criterion::Met,
        Ok(false) => Criterion::Unmet,
        Err(_) => Criterion::Indeterminate,
    }
}

pub fn observe_task_site_criteria(
    root: &Path,
    worktree: &Path,
    task_id: &str,
    recorded: &RecordedTasks,
) -> TaskSiteCriteria {
    let ledger_recorded = if recorded.by_task.contains_key(task_id) {
        Criterion::Met
    } else if recorded.skipped_rounds.is_empty() {
        Criterion::Unmet
    } else {
        Criterion::Indeterminate
    };
    TaskSiteCriteria::fail_closed()
        .with_ledger_recorded(ledger_recorded)
        .with_executor_process_absent(probe_executor_process_absent(root, task_id))
        .with_worktree_clean(probe_worktree_clean(worktree))
        .with_head_merged_into_main(probe_head_merged_into_main(root, worktree))
}

fn current_active_lease_sites(root: &Path) -> std::result::Result<Vec<crate::sites::Site>, String> {
    let round = crate::current_round(root).map_err(|error| format!("CURRENT-ROUND: {error:#}"))?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger = read_ledger(&ledger_path).map_err(|error| {
        format!(
            "read current lease ledger {}: {error:#}",
            ledger_path.display()
        )
    })?;
    if !ledger.bad_lines.is_empty() {
        return Err(format!(
            "current lease ledger has {} bad line(s)",
            ledger.bad_lines.len()
        ));
    }
    crate::sites::active_sites_checked(&ledger.events)
}

fn active_lease_blocker(
    root: &Path,
    worktree: &str,
) -> std::result::Result<Option<String>, String> {
    Ok(current_active_lease_sites(root)?
        .into_iter()
        .find(|site| site.worktree == worktree)
        .map(|site| site.site_id))
}

#[derive(Debug, Clone)]
pub struct TaskSiteVerdict {
    pub task_id: String,
    pub worktree: String,
    pub branch: String,
    pub criteria: TaskSiteCriteria,
    pub disposition: TaskSiteDisposition,
    pub freed_bytes: u64,
}

#[derive(Debug, Default)]
pub struct TaskSiteReclaimReport {
    pub verdicts: Vec<TaskSiteVerdict>,
    pub freed_bytes: u64,
    /// A destructive command failed after it may have partially changed a candidate.
    pub incomplete: bool,
}

fn repo_relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "worktree is outside repository root: root={} worktree={}",
            root.display(),
            path.display()
        )
    })?;
    let value = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    if value.is_empty() {
        bail!("repository-relative worktree path is empty");
    }
    Ok(value)
}

/// Reclaim only registered `.worktrees/<task>` entries whose current branch is exactly
/// `task/<task>`. Detached and otherwise indeterminate registry entries are outside this domain.
pub fn reclaim_task_sites(root: &Path) -> Result<TaskSiteReclaimReport> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("canonicalize repository root failed: {}", root.display()))?;
    let worktrees_root = root.join(".worktrees");
    let registry = gitx::worktree_registry(&root)?;
    let mut candidates = Vec::<(PathBuf, String, String, String)>::new();

    for entry in registry {
        let path = entry.path;
        if path.parent() != Some(worktrees_root.as_path()) {
            continue;
        }
        let Some(task_id) = path.file_name().and_then(OsStr::to_str).map(str::to_owned) else {
            continue;
        };
        let expected_branch = format!("task/{task_id}");
        match gitx::current_branch(&path) {
            Ok(Some(branch)) if branch == expected_branch => {
                let worktree = repo_relative_path(&root, &path)?;
                candidates.push((path, task_id, branch, worktree));
            }
            _ => continue,
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let recorded = tasks_recorded_in_any_round(&root).unwrap_or_else(|_| RecordedTasks {
        by_task: BTreeMap::new(),
        skipped_rounds: vec!["<ledger-read-failed>".to_string()],
    });

    let mut report = TaskSiteReclaimReport::default();
    for (path, task_id, branch, worktree) in candidates {
        let mut criteria = TaskSiteCriteria::fail_closed();
        let mut disposition = match active_lease_blocker(&root, &worktree) {
            Ok(Some(site_id)) => TaskSiteDisposition::Keep {
                reason: format!("active lease {site_id}"),
            },
            Err(reason) => TaskSiteDisposition::Keep {
                reason: format!("active_lease_evidence=Indeterminate ({reason})"),
            },
            Ok(None) => {
                criteria = observe_task_site_criteria(&root, &path, &task_id, &recorded);
                decide_task_site_reclaim(&criteria)
            }
        };
        let mut freed_bytes = 0u64;

        if disposition.is_reclaim() {
            match crate::buildcache::apparent_tree_bytes(&path) {
                Ok(measured) => {
                    criteria = observe_task_site_criteria(&root, &path, &task_id, &recorded);
                    disposition = decide_task_site_reclaim(&criteria);
                    if disposition.is_reclaim() {
                        disposition = match active_lease_blocker(&root, &worktree) {
                            Ok(Some(site_id)) => TaskSiteDisposition::Keep {
                                reason: format!("active lease {site_id}"),
                            },
                            Err(reason) => TaskSiteDisposition::Keep {
                                reason: format!("active_lease_evidence=Indeterminate ({reason})"),
                            },
                            Ok(None) => TaskSiteDisposition::Reclaim,
                        };
                    }
                    if disposition.is_reclaim() {
                        let identity_fresh = matches!(
                            gitx::current_branch(&path),
                            Ok(Some(current)) if current == branch
                        ) && matches!(
                            gitx::worktree_registry(&root),
                            Ok(entries) if entries.iter().any(|entry| entry.path == path && !entry.prunable)
                        );
                        if !identity_fresh {
                            disposition = TaskSiteDisposition::Keep {
                                reason: "candidate_identity=Indeterminate (fresh branch/registry mismatch)"
                                    .to_string(),
                            };
                        } else {
                            match gitx::worktree_remove(&root, &path) {
                                Ok(()) => {
                                    freed_bytes = measured;
                                    report.freed_bytes =
                                        report.freed_bytes.saturating_add(measured);
                                }
                                Err(remove_error) => {
                                    let path_missing = matches!(
                                        fs::symlink_metadata(&path),
                                        Err(error) if error.kind() == io::ErrorKind::NotFound
                                    );
                                    let registry_after = gitx::worktree_registry(&root);
                                    let removed_or_pruned = match registry_after {
                                        Ok(entries)
                                            if path_missing
                                                && !entries
                                                    .iter()
                                                    .any(|entry| entry.path == path) =>
                                        {
                                            true
                                        }
                                        Ok(entries)
                                            if path_missing
                                                && entries.iter().any(|entry| {
                                                    entry.path == path && entry.prunable
                                                }) =>
                                        {
                                            gitx::worktree_prune_exact(&root, &path).is_ok()
                                        }
                                        _ => false,
                                    };
                                    if removed_or_pruned {
                                        freed_bytes = measured;
                                        report.freed_bytes =
                                            report.freed_bytes.saturating_add(measured);
                                    } else {
                                        report.incomplete = true;
                                        disposition = TaskSiteDisposition::Keep {
                                            reason: format!(
                                                "worktree_remove=Indeterminate ({remove_error:#})"
                                            ),
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
                Err(error) => {
                    disposition = TaskSiteDisposition::Keep {
                        reason: format!("apparent_tree_bytes=Indeterminate ({error:#})"),
                    };
                }
            }
        }

        report.verdicts.push(TaskSiteVerdict {
            task_id,
            worktree,
            branch,
            criteria,
            disposition,
            freed_bytes,
        });
    }
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Governance {
    ReclaimedThisRound,
    GovernedResidual,
    Ungoverned,
}

#[derive(Debug, Clone)]
pub struct DiskGovernanceEntry {
    pub path: String,
    pub bytes: u64,
    pub governance: Governance,
}

impl DiskGovernanceEntry {
    pub fn new(path: &str, bytes: u64, governance: Governance) -> Self {
        Self {
            path: path.to_string(),
            bytes,
            governance,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskSummary {
    pub reclaimed_bytes: u64,
    pub occupied_bytes: u64,
    pub ungoverned_bytes: u64,
}

impl DiskSummary {
    pub fn render(&self) -> String {
        format!(
            "回收 freedBytes={} · 仍占用 occupiedBytes={} · 无人治理 ungovernedBytes={}",
            self.reclaimed_bytes, self.occupied_bytes, self.ungoverned_bytes
        )
    }
}

pub fn summarize_disk(entries: &[DiskGovernanceEntry]) -> DiskSummary {
    let mut summary = DiskSummary::default();
    for entry in entries {
        match entry.governance {
            Governance::ReclaimedThisRound => {
                summary.reclaimed_bytes = summary.reclaimed_bytes.saturating_add(entry.bytes);
            }
            Governance::GovernedResidual => {
                summary.occupied_bytes = summary.occupied_bytes.saturating_add(entry.bytes);
            }
            Governance::Ungoverned => {
                summary.occupied_bytes = summary.occupied_bytes.saturating_add(entry.bytes);
                summary.ungoverned_bytes = summary.ungoverned_bytes.saturating_add(entry.bytes);
            }
        }
    }
    summary
}

fn residual_governance(
    path: &str,
    retained_task_sites: &BTreeSet<String>,
    active_worktrees: &BTreeSet<String>,
    governed_targets: &BTreeSet<String>,
) -> Governance {
    if path == ".cowork-temp/trial-cache" || path == "orch/target/test-tmp" {
        Governance::GovernedResidual
    } else if retained_task_sites.contains(path)
        || active_worktrees.contains(path)
        || governed_targets.contains(path)
    {
        Governance::GovernedResidual
    } else {
        Governance::Ungoverned
    }
}

fn disk_survey_child_name(name: &OsStr) -> Result<&str> {
    name.to_str().context("disk survey child name is not UTF-8")
}

/// Survey direct children of `orch/target`, `.cowork-temp`, and `.worktrees`.
///
/// Classification is intentionally closed and defaults to [`Governance::Ungoverned`]: reclaimed
/// report paths are `ReclaimedThisRound`; trial-cache, test-tmp, TTL/Active-protected targets named
/// by a successful sweep, task sites retained by this reclaimer, and worktrees named by Active
/// leases are `GovernedResidual`; every other path remains `Ungoverned`.
pub fn survey_disk(
    root: &Path,
    reclaimed: &TaskSiteReclaimReport,
    swept: &crate::buildcache::SweepTargetsReport,
) -> Result<Vec<DiskGovernanceEntry>> {
    if reclaimed.incomplete || swept.incomplete {
        bail!("cleanup report is incomplete after a destructive command failure");
    }
    let mut entries = BTreeMap::<String, DiskGovernanceEntry>::new();
    let mut reclaimed_total = 0u64;
    let mut retained_task_sites = BTreeSet::<String>::new();

    for verdict in &reclaimed.verdicts {
        if verdict.disposition.is_reclaim() {
            reclaimed_total = reclaimed_total.saturating_add(verdict.freed_bytes);
            if entries
                .insert(
                    verdict.worktree.clone(),
                    DiskGovernanceEntry::new(
                        &verdict.worktree,
                        verdict.freed_bytes,
                        Governance::ReclaimedThisRound,
                    ),
                )
                .is_some()
            {
                bail!("task reclaim report contains a duplicate path");
            }
        } else {
            if verdict.freed_bytes != 0 {
                bail!("retained task-site verdict reports freed bytes");
            }
            retained_task_sites.insert(verdict.worktree.clone());
        }
    }
    if reclaimed_total != reclaimed.freed_bytes {
        bail!("task reclaim report byte accounting is inconsistent");
    }

    let removed_paths = swept.removed.iter().cloned().collect::<BTreeSet<_>>();
    if removed_paths.len() != swept.removed.len()
        || removed_paths.len() != swept.removed_bytes.len()
        || !removed_paths
            .iter()
            .all(|path| swept.removed_bytes.contains_key(path))
    {
        bail!("target sweep report path accounting is inconsistent");
    }
    let swept_total = swept
        .removed_bytes
        .values()
        .fold(0u64, |total, bytes| total.saturating_add(*bytes));
    if swept_total != swept.freed_bytes {
        bail!("target sweep report byte accounting is inconsistent");
    }
    for path in &swept.removed {
        let bytes = swept.removed_bytes[path];
        if entries
            .insert(
                path.clone(),
                DiskGovernanceEntry::new(path, bytes, Governance::ReclaimedThisRound),
            )
            .is_some()
        {
            bail!("cleanup reports contain the same reclaimed path");
        }
    }

    let preserved = swept.preserved.iter().cloned().collect::<BTreeSet<_>>();
    if !swept
        .governed_preserved
        .iter()
        .all(|path| preserved.contains(path))
    {
        bail!("target sweep governed-residual accounting is inconsistent");
    }
    let active_worktrees = swept
        .active_worktrees
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();

    for relative_root in ["orch/target", ".cowork-temp", ".worktrees"] {
        let absolute_root = root.join(relative_root);
        let metadata = match fs::symlink_metadata(&absolute_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("stat disk survey root failed: {}", absolute_root.display())
                })
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("disk survey root is not a real directory: {relative_root}");
        }
        let directory = match fs::read_dir(&absolute_root) {
            Ok(directory) => directory,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("read disk survey root failed: {}", absolute_root.display())
                })
            }
        };
        let mut children = directory.collect::<std::result::Result<Vec<_>, _>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            if !child.file_type()?.is_dir() {
                continue;
            }
            let child_name = child.file_name();
            let name = disk_survey_child_name(&child_name)?;
            let path = format!("{relative_root}/{name}");
            if entries.contains_key(&path) {
                bail!("a path reported as reclaimed still exists on disk: {path}");
            }
            let bytes = crate::buildcache::apparent_tree_bytes(&child.path())?;
            let governance = residual_governance(
                &path,
                &retained_task_sites,
                &active_worktrees,
                &swept.governed_preserved,
            );
            entries.insert(
                path.clone(),
                DiskGovernanceEntry::new(&path, bytes, governance),
            );
        }
    }

    Ok(entries.into_values().collect())
}

/// A deletion prerequisite. `passed: null` denotes an apply-time check, never a proven fact.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceCriterion {
    /// Stable prerequisite name.
    pub name: String,
    /// Observed result, or null for a lock/recheck deliberately not taken by a dry run.
    pub passed: Option<bool>,
    /// Concrete observation or required point-of-use recheck.
    pub detail: String,
}

/// One non-adopting maintenance observation or action result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceItem {
    /// Registered identity or a clearly labelled discovery/error identity.
    pub id: String,
    /// `site`, `diagnostic`, `unregistered`, `shared`, or `round`.
    pub kind: String,
    /// Physical boundaries; overlapping boundaries are counted only once in totals.
    pub paths: Vec<PathBuf>,
    /// `removed`, `held`, or `failed`; a dry run never reports a new removal.
    pub disposition: String,
    /// Retention, completion, or failure explanation.
    pub reason: String,
    /// Complete prerequisites, including checks deferred to the apply-time critical section.
    pub criteria: Vec<MaintenanceCriterion>,
    /// Current logical occupancy, or null when it cannot be measured safely.
    pub logical_bytes: Option<u64>,
    /// Newly deleted logical bytes only, excluding preserved quarantine moves and old receipts.
    pub removed_logical_bytes: u64,
    /// Observed physical prerequisites permit an attempt; not a deletion authorization.
    pub eligible: bool,
}

impl MaintenanceItem {
    pub(crate) fn held(id: String, kind: &str, paths: Vec<PathBuf>, reason: String) -> Self {
        // Report occupancy is measured once per unique boundary after all actions.
        // Constructors never rescan the same held cache for every historical alias.
        let logical_bytes = None;
        let criteria = if kind == "site" {
            [
                "canonical-no-symlink-paths",
                "trusted-release-and-not-blocked",
                "native-end-and-preserved-artifact",
                "fixed-clean-worktree-and-quarantine-bound",
                "journal-and-exact-registry",
                "last-user-absent",
                "logical-size",
                "apply-lock-and-directory-recheck",
                "all-round-ownership",
                "fresh-ledger-critical-section",
            ]
            .into_iter()
            .map(|name| MaintenanceCriterion {
                name: name.into(),
                passed: None,
                detail: "required; not yet evaluated or blocked by an earlier refusal".into(),
            })
            .collect()
        } else {
            Vec::new()
        };
        Self {
            id,
            kind: kind.into(),
            paths,
            disposition: "held".into(),
            reason,
            criteria,
            logical_bytes,
            removed_logical_bytes: 0,
            eligible: false,
        }
    }
    pub(crate) fn criterion(&mut self, name: &str, passed: Option<bool>, detail: String) {
        if let Some(criterion) = self.criteria.iter_mut().find(|c| c.name == name) {
            criterion.passed = passed;
            criterion.detail = detail;
        } else {
            self.criteria.push(MaintenanceCriterion {
                name: name.into(),
                passed,
                detail,
            });
        }
    }
}

/// Latest foreground storage maintenance result. Filesystem availability is not logical deletion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceReport {
    /// Report format version.
    pub schema_version: u32,
    /// UTC observation time; not a cache-age authorization.
    pub observed_at: String,
    /// True means no lock, report, ledger, WAL, registry, or cache mutation was performed.
    pub dry_run: bool,
    /// Optional round filter for legacy sites; diagnostic ownership remains independently checked.
    pub selected_round: Option<String>,
    /// Per-item outcomes and exact retention reasons.
    pub items: Vec<MaintenanceItem>,
    /// Available space before this operation, once per actual filesystem.
    pub filesystems_before: Vec<crate::storage::FilesystemSpace>,
    /// Fresh available space after this operation; concurrent activity may reduce it.
    pub filesystems_after: Vec<crate::storage::FilesystemSpace>,
    /// Union of safely measurable remaining paths, excluding ancestor/descendant double counting.
    pub measured_logical_bytes: u64,
    /// Number of union boundaries whose size remains unknown.
    pub unmeasured_boundaries: usize,
    /// Sum of this operation's non-overlapping actual logical deletions, never physical free space.
    pub removed_logical_bytes: u64,
    /// Failure to preserve the latest report; outcomes still remain available to the caller.
    pub report_error: Option<String>,
}

const MAINTENANCE_REPORT: &str = "coordination/runtime/storage-maintenance/latest.json";

/// Validate the exact path without following symlinks; a missing ancestor never shortens the result.
pub(crate) fn checked_storage_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || !relative
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        bail!("unsafe maintenance relative path: {}", relative.display());
    }
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("maintenance symlink: {}", cursor.display())
            }
            Ok(m) if cursor != root.join(relative) && !m.is_dir() => {
                bail!("maintenance ancestor is not a directory")
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(root.join(relative))
}

pub(crate) fn validate_storage_root(root: &Path) -> Result<()> {
    if !root.is_absolute() || fs::canonicalize(root)? != root {
        bail!("maintenance requires a canonical absolute root");
    }
    let marker = fs::symlink_metadata(root.join(".git"))?;
    if marker.file_type().is_symlink() || !(marker.is_file() || marker.is_dir()) {
        bail!("maintenance requires its own Git marker");
    }
    checked_storage_path(root, Path::new("coordination/runtime"))?;
    Ok(())
}

pub(crate) fn boundary_union(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = paths.to_vec();
    sorted.sort_by_key(|p| (p.components().count(), p.clone()));
    let mut result: Vec<PathBuf> = Vec::new();
    for path in sorted {
        if !result.iter().any(|parent| path.starts_with(parent)) {
            result.push(path);
        }
    }
    result
}

pub(crate) fn measure_boundaries(paths: &[PathBuf]) -> Result<u64> {
    let mut bytes = 0u64;
    for path in boundary_union(paths) {
        for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            match fs::symlink_metadata(ancestor) {
                Ok(m) if m.file_type().is_symlink() => {
                    bail!("symlink in logical-size boundary: {}", ancestor.display())
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("unsafe logical-size boundary: {}", path.display())
            }
            Ok(_) => {
                bytes = bytes
                    .checked_add(crate::buildcache::apparent_tree_bytes(&path)?)
                    .context("maintenance logical size overflow")?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(bytes)
}

#[derive(Clone)]
pub(crate) struct MaintenanceRound {
    pub round: String,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub events: Vec<EventRecord>,
    pub sites: Vec<crate::sites::Site>,
    pub error: Option<String>,
    pub watched: Vec<(PathBuf, Option<Vec<u8>>)>,
}

pub(crate) struct MaintenanceInventory {
    pub rounds: Vec<MaintenanceRound>,
    pub claims: Vec<(String, PathBuf)>,
    pub global_hold: Option<String>,
    pub errors: Vec<MaintenanceItem>,
}

fn safe_round_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn capture_claims(root: &Path, round: &str, bytes: &[u8], inventory: &mut MaintenanceInventory) {
    for (line, raw) in bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_slice::<serde_json::Value>(raw) {
            Ok(v) => v,
            Err(_) => {
                inventory.global_hold =
                    Some(format!("{round}: undecodable ownership line {}", line + 1));
                continue;
            }
        };
        let kind = value.get("type").and_then(|v| v.as_str());
        if kind != Some("WorkspaceLeased") {
            if kind.is_none() {
                inventory.global_hold = Some(format!("{round}: event kind/ownership unknown"));
            }
            continue;
        }
        let owner = value
            .pointer("/payload/siteId")
            .and_then(|v| v.as_str())
            .map(|id| format!("{round}/{id}"))
            .unwrap_or_else(|| format!("{round}/unknown-{line}"));
        for key in ["worktree", "target"] {
            let path = value
                .pointer(&format!("/payload/paths/{key}"))
                .and_then(|v| v.as_str());
            match path.map(Path::new).filter(|p| {
                !p.as_os_str().is_empty()
                    && !p.is_absolute()
                    && p.components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
            }) {
                Some(path) => inventory.claims.push((owner.clone(), root.join(path))),
                None => {
                    inventory.global_hold =
                        Some(format!("{round}: unbounded lease {key} ownership"))
                }
            }
        }
    }
}

/// Snapshot only: corrupt rounds retain their claims instead of disappearing from the deletion domain.
pub(crate) fn storage_inventory(root: &Path) -> MaintenanceInventory {
    let mut inventory = MaintenanceInventory {
        rounds: Vec::new(),
        claims: Vec::new(),
        global_hold: None,
        errors: Vec::new(),
    };
    let directory = match checked_storage_path(root, Path::new("coordination/rounds"))
        .and_then(|path| fs::read_dir(path).map_err(Into::into))
    {
        Ok(d) => d,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return inventory
        }
        Err(error) => {
            inventory.global_hold = Some(format!("round inventory unavailable: {error:#}"));
            return inventory;
        }
    };
    let mut entries = directory
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap_or_else(|error| {
            inventory.global_hold = Some(format!("round directory read failed: {error}"));
            Vec::new()
        });
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        // Only confirmed regular files are non-round metadata. Classify before
        // applying directory identity rules; never exempt a directory/link by name.
        match entry.file_type() {
            Ok(kind) if kind.is_file() => continue,
            Ok(_) => {},
            Err(error) => {
                inventory.global_hold = Some(format!("round entry type unavailable: {}: {error}", entry.path().display()));
                continue;
            }
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !safe_round_name(&name) {
            inventory.global_hold = Some(format!("unsafe round directory identity: {name}"));
            continue;
        }
        let path = root.join(format!("coordination/rounds/{name}/events.jsonl"));
        let read = (|| -> Result<(Vec<u8>, Vec<EventRecord>, Vec<crate::sites::Site>, Option<String>, Vec<(PathBuf, Option<Vec<u8>>)>)> {
            checked_storage_path(root, path.strip_prefix(root)?)?;
            let bytes = bounded_storage_read(&path, 64 * 1024 * 1024)?;
            capture_claims(root, &name, &bytes, &mut inventory);
            let ledger = read_ledger(&path)?;
            let (sites, mut error) = crate::sites::maintenance_sites(&ledger.events);
            let closed = ledger.events.iter().any(|event| event.kind == "RoundClosed" && event.actor == "runtime:orch" && event.round.as_deref() == Some(name.as_str()));
            let live_v3 = ledger.events.iter().any(|event| event.kind == "RoundOpened" && event.actor == "runtime:orch" && event.payload.as_ref().and_then(|p| p.get("contractSchemaVersion")).and_then(|v| v.as_u64()) == Some(3));
            if !closed && !live_v3 {
                error = Some("schema 1/2 active round 只读兼容；只有可信闭轮后才允许独立存储维护".into());
            }
            if !ledger.bad_lines.is_empty() || ledger.events.iter().any(|e| e.round.as_deref() != Some(&name)) {
                error = Some("malformed or cross-round ledger evidence".into());
            }
            let wal = checked_storage_path(root, Path::new(&format!("coordination/runtime/ledger-wal/{name}.jsonl")))?;
            let mut watched = Vec::new();
            match bounded_storage_read(&wal, 64 * 1024 * 1024) {
                Ok(wal_bytes) if wal_bytes != bytes => {
                    capture_claims(root, &name, &wal_bytes, &mut inventory);
                    watched.push((wal.clone(), Some(wal_bytes)));
                    error = Some("ledger/WAL divergence; maintenance never repairs history".into());
                }
                Ok(wal_bytes) => watched.push((wal.clone(), Some(wal_bytes))),
                Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => watched.push((wal.clone(), None)),
                Err(e) => { inventory.global_hold = Some(format!("{name}: unreadable WAL ownership")); error = Some(format!("{e:#}")); }
            }
            let intent = checked_storage_path(root, Path::new(&format!("coordination/runtime/ledger-wal/.{name}.atomic-batch-intent.json")))?;
            match fs::symlink_metadata(&intent) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => watched.push((intent.clone(), None)),
                _ => { inventory.global_hold = Some(format!("{name}: unresolved atomic intent; no cleanup repair")); error = inventory.global_hold.clone(); }
            }
            Ok((bytes, ledger.events, sites, error, watched))
        })();
        match read {
            Ok((bytes, events, sites, error, watched)) => inventory.rounds.push(MaintenanceRound {
                round: name,
                path,
                bytes,
                events,
                sites,
                error,
                watched,
            }),
            Err(error) => {
                inventory.global_hold =
                    Some(format!("{name}: ownership could not be completely bounded"));
                let mut item = MaintenanceItem::held(
                    format!("round:{name}"),
                    "round",
                    Vec::new(),
                    format!("{error:#}"),
                );
                item.disposition = "failed".into();
                inventory.errors.push(item);
            }
        }
    }
    inventory
}

pub(crate) fn ownership_refusal(
    root: &Path,
    inventory: &MaintenanceInventory,
    round: &str,
    site: &crate::sites::Site,
) -> Option<String> {
    ownership_refusal_for_members(root, inventory, round, std::slice::from_ref(site))
}

fn ownership_refusal_for_members(
    root: &Path,
    inventory: &MaintenanceInventory,
    round: &str,
    members: &[crate::sites::Site],
) -> Option<String> {
    if let Some(reason) = &inventory.global_hold {
        return Some(reason.clone());
    }
    let site = &members[0];
    let owners = members.iter().map(|s| format!("{round}/{}", s.site_id)).collect::<BTreeSet<_>>();
    for path in [&site.worktree, &site.target] {
        let absolute = root.join(path);
        if let Some((other, claim)) = inventory.claims.iter().find(|(other, claim)| {
            !owners.contains(other) && (absolute.starts_with(claim) || claim.starts_with(&absolute))
        }) {
            return Some(format!("overlapping owner {other}: {}", claim.display()));
        }
    }
    None
}

// Re-read the original claim domain at the effect boundary, including newly added rounds.
pub(crate) fn maintenance_inventory_unchanged(root: &Path, before: &MaintenanceInventory) -> bool {
    let after = storage_inventory(root);
    after.global_hold == before.global_hold
        && after.claims == before.claims
        && after.rounds.len() == before.rounds.len()
        && after.errors.len() == before.errors.len()
        && after.rounds.iter().zip(&before.rounds).all(|(a, b)| {
            a.round == b.round && a.path == b.path && a.bytes == b.bytes
                && a.error == b.error && a.watched == b.watched
        })
}

fn bounded_storage_read(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() > maximum {
        bail!("unsafe or oversized maintenance input: {}", path.display());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        bail!("maintenance input grew beyond bound");
    }
    Ok(bytes)
}

/// Keep the existing shared-protocol → ledger order, without append_checked's repair/event effects.
pub(crate) fn with_maintenance_ledger<T>(
    root: &Path,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    validate_storage_root(root)?;
    crate::close::with_protocol_ledger_effect(root, "storage maintenance", || {
        let directory = checked_storage_path(root, Path::new("coordination/runtime/locks"))?;
        fs::create_dir_all(&directory)?;
        let path = checked_storage_path(root, Path::new("coordination/runtime/locks/ledger.lock"))?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut lock = fd_lock::RwLock::new(file);
        let _guard = lock
            .try_write()
            .context("maintenance ledger lock busy; no repairs attempted")?;
        action()
    })
}

fn diagnostic_item(view: crate::buildcache::DiagnosticCacheView, dry_run: bool) -> MaintenanceItem {
    let mut item = MaintenanceItem::held(
        format!("diagnostic:{}", view.id),
        "diagnostic",
        vec![view.path],
        view.reason.clone(),
    );
    item.disposition = if dry_run {
        "held".into()
    } else {
        view.disposition
    };
    item.eligible = view.eligible;
    item.logical_bytes = view.logical_bytes;
    item.removed_logical_bytes = if dry_run {
        0
    } else {
        view.removed_logical_bytes
    };
    item.criterion(
        "diagnostic-owned-terminal-evidence",
        if view.eligible { Some(true) } else { None },
        view.reason,
    );
    item.criterion("diagnostic-required-checks", None, "registered ID/root/HEAD, dev+inode, actual wait, stable logs, unchanged clean source, keep marker and process/open-file quiescence; absent receipt recovery has no deletion".into());
    item.criterion(
        "exclusive-entry-lease",
        if dry_run { None } else { Some(true) },
        "apply takes the existing entry lease and repeats eligibility; dry-run takes no lock"
            .into(),
    );
    item
}

fn inspect_legacy(root: &Path, selected: Option<&str>, dry_run: bool) -> Vec<MaintenanceItem> {
    let inventory = storage_inventory(root);
    let mut items = inventory.errors.clone();
    let registered = gitx::worktree_registry(root)
        .ok()
        .map(|entries| entries.into_iter().map(|e| e.path).collect::<BTreeSet<_>>());
    if let Some(reason) = &inventory.global_hold {
        let mut item = MaintenanceItem::held(
            "legacy:unbounded".into(),
            "round",
            Vec::new(),
            reason.clone(),
        );
        item.disposition = "failed".into();
        items.push(item);
    }
    for round in &inventory.rounds {
        if selected.is_some_and(|name| name != round.round) {
            continue;
        }
        if let Some(reason) = &round.error {
            let mut item = MaintenanceItem::held(
                format!("round:{}", round.round),
                "round",
                Vec::new(),
                reason.clone(),
            );
            item.disposition = "failed".into();
            items.push(item);
        }
        let mut visited = BTreeSet::new();
        for site in &round.sites {
            if !visited.insert(site.site_id.clone()) {
                continue;
            }
            let members = round.sites.iter().filter(|peer| {
                site.role == crate::sites::SiteRole::Implement
                    && peer.identity() == site.identity()
                    && peer.worktree == site.worktree && peer.target == site.target
            }).cloned().collect::<Vec<_>>();
            if members.len() > 1 {
                visited.extend(members.iter().map(|s| s.site_id.clone()));
                // Claims remain in the original inventory even when all physical paths are gone.
                let absent = [&site.worktree, &site.target].iter().all(|p| {
                    matches!(fs::symlink_metadata(root.join(p)), Err(e) if e.kind() == io::ErrorKind::NotFound)
                });
                if absent && registered.as_ref().is_some_and(|p| !p.contains(&root.join(&site.worktree))) {
                    continue;
                }
                let refusal = round.error.clone().or_else(|| {
                    ownership_refusal_for_members(root, &inventory, &round.round, &members)
                });
                if let Some(reason) = refusal {
                    for member in &members {
                        items.push(MaintenanceItem::held(format!("{}/{}", round.round, member.site_id),
                            "site", vec![root.join(&member.worktree), root.join(&member.target)], reason.clone()));
                    }
                } else {
                    let unchanged = || maintenance_inventory_unchanged(root, &inventory);
                    items.extend(crate::sites::maintain_equivalent_implementations(
                        root, &round.round, &round.events, &members, dry_run, &unchanged));
                }
                continue;
            }
            // Keep every historical claim in the conflict map, but do not re-audit
            // already-absent physical sites on every fresh round opening.
            let absent = [&site.worktree, &site.target].iter().all(|p| {
                matches!(
                fs::symlink_metadata(root.join(p)), Err(e) if e.kind() == io::ErrorKind::NotFound)
            });
            if absent
                && registered
                    .as_ref()
                    .is_some_and(|paths| !paths.contains(&root.join(&site.worktree)))
            {
                continue;
            }
            let refusal = round
                .error
                .clone()
                .or_else(|| ownership_refusal(root, &inventory, &round.round, site));
            if let Some(reason) = refusal {
                items.push(MaintenanceItem::held(
                    format!("{}/{}", round.round, site.site_id),
                    "site",
                    vec![root.join(&site.worktree), root.join(&site.target)],
                    reason,
                ));
                continue;
            }
            let stable = inventory.rounds.iter().all(|r| bounded_storage_read(&r.path, 64 * 1024 * 1024).is_ok_and(|b| b == r.bytes)
                && r.watched.iter().all(|(path, before)| match before {
                    Some(before) => bounded_storage_read(path, 64 * 1024 * 1024).is_ok_and(|after| &after == before),
                    None => matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound),
                }));
            if !stable {
                items.push(MaintenanceItem::held(format!("{}/{}", round.round, site.site_id), "site", Vec::new(),
                    "ledger changed outside the held writer lock; remaining legacy deletion refused".into()));
                continue;
            }
            let mut item =
                crate::sites::maintain_one_site(root, &round.round, &round.events, site, dry_run);
            item.criterion(
                "all-round-ownership",
                Some(true),
                "all decoded claims, including terminal generations, checked for overlap".into(),
            );
            item.criterion("fresh-ledger-critical-section", if dry_run { None } else { Some(true) }, "dry-run is an unlocked snapshot; apply re-reads under the existing writer lock without repair".into());
            items.push(item);
        }
    }
    items
}

fn add_unregistered_inventory(root: &Path, items: &mut Vec<MaintenanceItem>) {
    let owned = items
        .iter()
        .flat_map(|i| i.paths.clone())
        .collect::<Vec<_>>();
    for relative in ["orch/target", ".worktrees", ".cowork-temp"] {
        let parent = match checked_storage_path(root, Path::new(relative)) {
            Ok(p) => p,
            Err(e) => {
                items.push(MaintenanceItem::held(
                    format!("unregistered:{relative}"),
                    "unregistered",
                    Vec::new(),
                    format!("{e:#}"),
                ));
                continue;
            }
        };
        let entries = match fs::read_dir(&parent) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                items.push(MaintenanceItem::held(
                    format!("unregistered:{relative}"),
                    "unregistered",
                    Vec::new(),
                    e.to_string(),
                ));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    items.push(MaintenanceItem::held(
                        format!("unregistered:{relative}:entry-error"),
                        "unregistered",
                        vec![parent.clone()],
                        format!("directory inventory incomplete: {error}"),
                    ));
                    continue;
                }
            };
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_file() => continue,
                Ok(_) => {},
                Err(error) => {
                    items.push(MaintenanceItem::held(format!("unregistered:{}", path.display()),
                        "unregistered", vec![path], format!("cache boundary type unavailable: {error}")));
                    continue;
                }
            }
            if owned.iter().any(|p| path.starts_with(p)) {
                continue;
            }
            let shared = relative == "orch/target" && entry.file_name() == "debug";
            items.push(MaintenanceItem::held(
                format!(
                    "{}:{}",
                    if shared { "shared" } else { "unregistered" },
                    path.strip_prefix(root).unwrap_or(&path).display()
                ),
                if shared { "shared" } else { "unregistered" },
                vec![path],
                if shared {
                    "ordinary main build cache is intentionally retained"
                } else {
                    "no exclusive registered ownership; names and age never authorize deletion"
                }
                .into(),
            ));
        }
    }
}

fn save_maintenance_report(root: &Path, report: &MaintenanceReport) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = checked_storage_path(root, Path::new(MAINTENANCE_REPORT))?;
    let parent = path.parent().context("maintenance report has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".report-{}.tmp", ulid::Ulid::new()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Read the latest stored maintenance result without creating directories or repairing history.
pub fn latest_maintenance_report(root: &Path) -> Result<Option<MaintenanceReport>> {
    validate_storage_root(root)?;
    let path = checked_storage_path(root, Path::new(MAINTENANCE_REPORT))?;
    let bytes = match bounded_storage_read(&path, 16 * 1024 * 1024) {
        Ok(b) => b,
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e),
    };
    let report: MaintenanceReport = serde_json::from_slice(&bytes)?;
    if report.schema_version != 1 {
        bail!("unknown maintenance report schema");
    }
    Ok(Some(report))
}

/// Render foreground outcomes with explicitly logical bytes and independent filesystem readings.
pub fn maintenance_summary(report: &MaintenanceReport) -> Vec<String> {
    let count = |state: &str| {
        report
            .items
            .iter()
            .filter(|i| i.disposition == state)
            .count()
    };
    let eligible_not_reclaimed = if report.dry_run { 0 } else {
        report.items.iter().filter(|i| i.eligible && i.disposition != "removed").count()
    };
    let mut lines = vec![format!("storage maintenance: dryRun={} removed={} held={} failed={} eligibleNotReclaimed={} logicalDeletedBytes={} measuredRemainingBytes={} unmeasuredBoundaries={}",
        report.dry_run, count("removed"), count("held"), count("failed"), eligible_not_reclaimed, report.removed_logical_bytes, report.measured_logical_bytes, report.unmeasured_boundaries),
        format!("filesystem available before={:?} after={:?}", report.filesystems_before, report.filesystems_after)];
    for item in &report.items {
        if item.disposition == "failed"
            || item.kind == "site"
            || item.kind == "diagnostic"
            || item.logical_bytes.is_some_and(|n| n >= 64 * 1024 * 1024)
        {
            lines.push(format!(
                "{} {} eligible={} logicalBytes={:?} logicalDeletedBytes={}: {}",
                item.disposition,
                item.id,
                item.eligible,
                item.logical_bytes,
                item.removed_logical_bytes,
                item.reason
            ));
        }
    }
    if let Some(error) = &report.report_error {
        lines.push(format!("failed latest-maintenance-report: {error}"));
    }
    lines
}

/// Preserve one diagnostic operation's maintenance result without triggering unrelated cleanup.
/// Space readings cover the operation; they are never described as bytes physically freed by it.
pub fn record_diagnostic_maintenance(
    root: &Path,
    view: crate::buildcache::DiagnosticCacheView,
    before: Vec<crate::storage::FilesystemSpace>,
) -> MaintenanceReport {
    finish_maintenance_report(
        root,
        None,
        false,
        vec![diagnostic_item(view, false)],
        before,
        vec![
            root.to_path_buf(),
            root.join("coordination/runtime/diagnostic-cache"),
        ],
    )
}

/// Retry only registered diagnostics and preserve a unified, read-back maintenance report.
/// This path does not reclaim unrelated sites or require an active round.
pub fn maintain_diagnostic_storage(root: &Path) -> Result<MaintenanceReport> {
    validate_storage_root(root)?;
    let mut paths = vec![
        root.to_path_buf(),
        root.join("coordination/runtime/diagnostic-cache"),
    ];
    if let Ok(views) = crate::buildcache::diagnostic_cache_records(root) {
        paths.extend(views.into_iter().map(|v| v.path));
    }
    let before = crate::storage::observe_filesystem_space(&paths);
    let items = match crate::buildcache::sweep_managed_diagnostics(root) {
        Ok(views) => views
            .into_iter()
            .map(|v| diagnostic_item(v, false))
            .collect(),
        Err(error) => {
            let mut item = MaintenanceItem::held(
                "diagnostic:registry".into(),
                "diagnostic",
                vec![root.join("coordination/runtime/diagnostic-cache")],
                format!("{error:#}"),
            );
            item.disposition = "failed".into();
            vec![item]
        }
    };
    Ok(finish_maintenance_report(
        root, None, false, items, before, paths,
    ))
}

/// Observe the real main-target filesystems before a target-only maintenance operation.
pub fn target_maintenance_space(root: &Path) -> Result<Vec<crate::storage::FilesystemSpace>> {
    validate_storage_root(root)?;
    let target = checked_storage_path(root, Path::new("orch/target"))?;
    let mut paths = vec![root.to_path_buf(), target.clone()];
    match fs::read_dir(target) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_symlink() {
                    paths.push(entry.path());
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(crate::storage::observe_filesystem_space(&paths))
}

/// Preserve target-only outcomes in the same report format, without broadening the deletion scope.
pub fn record_target_maintenance(
    root: &Path,
    outcome: &crate::buildcache::SweepTargetsReport,
    before: Vec<crate::storage::FilesystemSpace>,
) -> MaintenanceReport {
    let names = outcome
        .removed
        .iter()
        .chain(&outcome.refused)
        .chain(&outcome.preserved)
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut items = Vec::new();
    for name in names {
        let mut item = MaintenanceItem::held(
            format!("target:{name}"),
            "site",
            vec![root.join(&name)],
            "unregistered, active, overlapping, or otherwise ineligible; name/age is not authority"
                .into(),
        );
        item.removed_logical_bytes = *outcome.removed_bytes.get(&name).unwrap_or(&0);
        if outcome.removed.contains(&name) {
            item.disposition = "removed".into();
            item.reason =
                "target-only removal after global ownership and physical preflight".into();
        }
        if let Some(failure) = outcome
            .failures
            .iter()
            .find(|f| f.starts_with(&format!("{name}:")))
        {
            item.disposition = "failed".into();
            item.reason = failure.clone();
        }
        items.push(item);
    }
    finish_maintenance_report(
        root,
        None,
        false,
        items,
        before,
        vec![root.to_path_buf(), root.join("orch/target")],
    )
}

fn finish_maintenance_report(
    root: &Path,
    selected: Option<&str>,
    dry_run: bool,
    mut items: Vec<MaintenanceItem>,
    before: Vec<crate::storage::FilesystemSpace>,
    mut paths: Vec<PathBuf>,
) -> MaintenanceReport {
    items.sort_by(|a, b| a.id.cmp(&b.id));
    let mut measured_logical_bytes = 0u64;
    let mut unmeasured_boundaries = 0usize;
    // This cache is local to this final report snapshot. It is never reused by
    // deletion preflight, before/after deletion measurements, admission, or a later call.
    let boundaries = items.iter().flat_map(|item| boundary_union(&item.paths)).collect::<BTreeSet<_>>();
    let measured = boundaries.into_iter().map(|path| {
        let bytes = measure_boundaries(std::slice::from_ref(&path)).ok();
        (path, bytes)
    }).collect::<BTreeMap<_, _>>();
    for item in &mut items {
        item.logical_bytes = boundary_union(&item.paths).into_iter().try_fold(0u64, |sum, path| {
            sum.checked_add(measured.get(&path).copied().flatten()?)
        });
    }
    for path in boundary_union(
        &items
            .iter()
            .flat_map(|i| i.paths.clone())
            .collect::<Vec<_>>(),
    ) {
        match measured.get(&path).copied().flatten() {
            Some(n) => measured_logical_bytes = measured_logical_bytes.saturating_add(n),
            None => unmeasured_boundaries += 1,
        }
    }
    let removed_logical_bytes = items.iter().fold(0u64, |sum, item| {
        sum.saturating_add(item.removed_logical_bytes)
    });
    paths.extend(items.iter().flat_map(|i| i.paths.clone()));
    let after = crate::storage::observe_filesystem_space(&paths);
    let mut report = MaintenanceReport {
        schema_version: 1,
        observed_at: crate::util::now_rfc3339(),
        dry_run,
        selected_round: selected.map(str::to_string),
        items,
        filesystems_before: before,
        filesystems_after: after,
        measured_logical_bytes,
        unmeasured_boundaries,
        removed_logical_bytes,
        report_error: None,
    };
    if !dry_run {
        if let Err(e) =
            validate_storage_root(root).and_then(|_| save_maintenance_report(root, &report))
        {
            report.report_error = Some(format!("{e:#}"));
        }
    }
    report
}

/// Maintain registered caches and released sites across rounds, including after a round has closed.
/// Same-round equivalent implementation generations share one physical removal only after every
/// owner passes the existing release, evidence, identity and quiet-use checks. Other overlapping
/// claims and reappeared complete journals remain held; aliases never claim deletion or write journals.
/// A proved dedicated Git fsmonitor may be stopped through native IPC during apply only.
/// A dry-run reports pending stop, never quiet or deletion; uncertain/shared monitors remain held.
/// Native-stop effects survive later failures; fresh all-owner checks precede stop and removal.
/// Dry-run is strictly read-only; apply uses fresh ownership under locks and never writes a ledger.
pub fn maintain_storage(root: &Path, dry_run: bool) -> Result<MaintenanceReport> {
    maintain_storage_for_round(root, None, dry_run)
}

/// The same maintenance with an optional legacy-round filter; all rounds still protect ownership.
/// Diagnostic retries remain independent, and cleanup failures never synthesize task completion.
pub fn maintain_storage_for_round(
    root: &Path,
    selected: Option<&str>,
    dry_run: bool,
) -> Result<MaintenanceReport> {
    validate_storage_root(root)?;
    if selected.is_some_and(|r| !safe_round_name(r)) {
        bail!("unsafe maintenance round filter");
    }
    let mut observed = inspect_legacy(root, selected, true);
    let diagnostics = crate::buildcache::diagnostic_cache_records(root);
    match diagnostics {
        Ok(views) => observed.extend(views.into_iter().map(|v| diagnostic_item(v, true))),
        Err(e) => {
            let mut item = MaintenanceItem::held(
                "diagnostic:registry".into(),
                "diagnostic",
                Vec::new(),
                format!("{e:#}"),
            );
            item.disposition = "failed".into();
            observed.push(item);
        }
    }
    add_unregistered_inventory(root, &mut observed);
    let mut paths = observed
        .iter()
        .flat_map(|i| i.paths.clone())
        .collect::<Vec<_>>();
    paths.extend([
        root.to_path_buf(),
        root.join("coordination/runtime/diagnostic-cache"),
    ]);
    let before = crate::storage::observe_filesystem_space(&paths);
    let items = if dry_run {
        observed
    } else {
        let mut result = Vec::new();
        // The independent diagnostic namespace never enters the legacy ledger critical section.
        match crate::buildcache::sweep_managed_diagnostics(root) {
            Ok(views) => result.extend(views.into_iter().map(|v| diagnostic_item(v, false))),
            Err(e) => {
                let mut item = MaintenanceItem::held(
                    "diagnostic:registry".into(),
                    "diagnostic",
                    Vec::new(),
                    format!("{e:#}"),
                );
                item.disposition = "failed".into();
                result.push(item);
            }
        }
        match with_maintenance_ledger(root, || Ok(inspect_legacy(root, selected, false))) {
            Ok(legacy) => result.extend(legacy),
            Err(e) => {
                let mut item = MaintenanceItem::held(
                    "legacy:lock".into(),
                    "round",
                    Vec::new(),
                    format!("{e:#}"),
                );
                item.disposition = "failed".into();
                result.push(item);
            }
        }
        add_unregistered_inventory(root, &mut result);
        result
    };
    Ok(finish_maintenance_report(
        root, selected, dry_run, items, before, paths,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_test(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn maintenance_fixture(tag: &str) -> (PathBuf, crate::sites::Site, Vec<EventRecord>) {
        use sha2::Digest;
        let root = crate::util::test_scratch_dir(tag);
        git_test(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("tracked.txt"), b"source\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\norch/target/\ncoordination/\n",
        )
        .unwrap();
        git_test(&root, &["add", "."]);
        git_test(&root, &["commit", "-qm", "fixture"]);
        let head = git_test(&root, &["rev-parse", "HEAD"]);
        let site = crate::sites::Site {
            site_id: "M1-review-probe-g01".into(),
            generation: 1,
            task_id: "M1".into(),
            attempt_id: "M1-A0001".into(),
            role: crate::sites::SiteRole::Review,
            agent: "probe".into(),
            reviewed_head: head.clone(),
            worktree: ".worktrees/review-M1-A0001-review-probe-g01".into(),
            target: "orch/target/review-M1-A0001-review-probe-g01".into(),
            wake_id: Some("wake-M1".into()),
        };
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        git_test(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                root.join(&site.worktree).to_str().unwrap(),
                &head,
            ],
        );
        fs::create_dir_all(root.join(&site.target)).unwrap();
        fs::write(root.join(&site.target).join("cache"), b"owned-cache").unwrap();
        let evidence = root.join("coordination/runtime/review-inbox/rMaint/M1.md");
        fs::create_dir_all(evidence.parent().unwrap()).unwrap();
        fs::write(&evidence, b"preserved native answer").unwrap();
        let event = |kind: &str, payload| {
            crate::ledger::event(kind, "runtime:orch", Some("M1"), Some("rMaint"), payload)
        };
        let lease = event(
            "WorkspaceLeased",
            serde_json::json!({"siteId":site.site_id,"generation":1,
            "attemptId":site.attempt_id,"role":"review","agent":"probe","reviewedHead":head,
            "wakeId":"wake-M1","paths":{"worktree":site.worktree,"target":site.target}}),
        );
        let terminal = event(
            "ManagedWakeTerminated",
            serde_json::json!({"wakeId":"wake-M1","agent":"probe",
            "managedScopeTerminated":true,"turnEnded":true,"state":"answered","mechanicalTerminalAbsent":false,
            "channelBinding":{"fixedHead":head},"outputPath":evidence,
            "outputSha256":hex::encode(sha2::Sha256::digest(b"preserved native answer"))}),
        );
        let release = event(
            "WorkspaceReleased",
            serde_json::json!({"siteId":site.site_id,"generation":1,
            "attemptId":site.attempt_id,"role":"review","agent":"probe","wakeId":"wake-M1",
            "completionReceipt":crate::sites::MANAGED_COMPLETION_RECEIPT,"terminationEventId":terminal.event_id}),
        );
        let closed = crate::ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some("rMaint"),
            serde_json::json!({"forced":false}),
        );
        let events = vec![lease, terminal, release, closed];
        write_maintenance_events(&root, "rMaint", &events);
        (root, site, events)
    }

    fn retired_group_fixture() -> (PathBuf, Vec<crate::sites::Site>, Vec<EventRecord>) {
        let (root, review, _) = maintenance_fixture("equivalent-group-recovery");
        git_test(&root, &["worktree", "remove", "--force", &review.worktree]);
        let report = "coordination/rounds/rMaint/reports/M1-REPORT.md";
        fs::create_dir_all(root.join(report).parent().unwrap()).unwrap();
        fs::write(root.join(report), b"preserved implementation report").unwrap();
        git_test(&root, &["add", "-f", report]);
        git_test(&root, &["commit", "-qm", "fixed report"]);
        let head = git_test(&root, &["rev-parse", "HEAD"]);
        git_test(&root, &["worktree", "add", "--detach", ".worktrees/M1", &head]);
        fs::create_dir_all(root.join(".worktrees/M1/orch/target")).unwrap();
        fs::write(root.join(".worktrees/M1/orch/target/cache"), b"reclaim once").unwrap();
        let mut members = Vec::new();
        let mut events = Vec::new();
        for generation in 1..=2 {
            let site = crate::sites::Site { site_id: format!("M1-implement-local-g{generation:02}"),
                generation, task_id: "M1".into(), attempt_id: format!("M1-A{generation:04}"),
                role: crate::sites::SiteRole::Implement, agent: "local".into(), reviewed_head: head.clone(),
                worktree: ".worktrees/M1".into(), target: ".worktrees/M1/orch/target".into(), wake_id: None };
            events.push(crate::ledger::event("WorkspaceLeased", "runtime:orch", Some("M1"), Some("rMaint"),
                serde_json::json!({"siteId":site.site_id,"generation":generation,"attemptId":site.attempt_id,
                "role":"implement","agent":"local","reviewedHead":head,"paths":{"worktree":site.worktree,"target":site.target}})));
            members.push(site);
        }
        events.push(crate::ledger::event("MergeStarted", "runtime:orch", Some("M1"), Some("rMaint"), serde_json::json!({"headSha":head})));
        let recorded = crate::ledger::event("TaskRecorded", "runtime:orch", Some("M1"), Some("rMaint"), serde_json::json!({"postMergeGates":"all-green"}));
        let retirements = crate::sites::retire_task_sites(&events, "M1", &recorded.event_id);
        events.push(recorded);
        events.extend(retirements);
        events.push(crate::ledger::event("RoundClosed", "runtime:orch", None, Some("rMaint"), serde_json::json!({"forced":false})));
        write_maintenance_events(&root, "rMaint", &events);
        (root, members, events)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_monitor_group_aba_holds_before_any_stop() {
        struct Stop(PathBuf);impl Drop for Stop {fn drop(&mut self){let _=Command::new("git").args(["fsmonitor--daemon","stop"]).current_dir(&self.0).output();}}
        let (root,members,events)=retired_group_fixture();let wt=root.join(&members[0].worktree);let _stop=Stop(wt.clone());
        assert!(Command::new("git").args(["-c","core.fsmonitor=true","fsmonitor--daemon","start"]).current_dir(&wt).env("HOME", &wt).output().unwrap().status.success());
        let p=root.join(format!("coordination/runtime/site-cleanup/rMaint/{}.json",members[0].site_id));fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p,serde_json::to_vec(&serde_json::json!({"version":1,"round":"rMaint","site":members[0],"phase":"complete","quarantine":null,"note":null})).unwrap()).unwrap();
        let result=crate::sites::maintain_equivalent_implementations(&root,"rMaint",&events,&members,false,&||true);
        assert!(result.iter().all(|i|!i.eligible));assert!(wt.exists());
        assert!(Command::new("git").args(["fsmonitor--daemon","status"]).current_dir(&wt).output().unwrap().status.success());
    }

    #[test]
    fn equivalent_group_resumes_only_representative_journal_and_counts_failure_honestly() {
        use std::os::unix::fs::PermissionsExt;
        let (root, members, events) = retired_group_fixture();
        let representative = &members[1];
        let parent = root.join("coordination/runtime/site-cleanup/rMaint");
        fs::create_dir_all(&parent).unwrap();
        let journal = parent.join(format!("{}.json", representative.site_id));
        fs::write(&journal, serde_json::to_vec(&serde_json::json!({"version":1,"round":"rMaint",
            "site":representative,"phase":"worktree-removed","quarantine":null,"note":null})).unwrap()).unwrap();
        let initial = fs::read(&journal).unwrap();
        let expected = measure_boundaries(&[root.join(&representative.worktree)]).unwrap();
        // A real journal persistence failure must not turn into an alias success or deletion claim.
        // Root can override DAC; that environment still exercises the interrupted-journal replay below.
        let uid = Command::new("id").arg("-u").output().unwrap();
        if String::from_utf8_lossy(&uid.stdout).trim() != "0" {
            let permissions = fs::metadata(&parent).unwrap().permissions();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
            let failed = maintain_storage(&root, false);
            fs::set_permissions(&parent, permissions).unwrap();
            let failed = failed.unwrap();
            assert!(failed.items.iter().any(|i| i.id.ends_with(&representative.site_id) && i.disposition == "failed"));
            assert_eq!(failed.removed_logical_bytes, 0);
            assert!(root.join(&representative.worktree).exists());
            assert_eq!(fs::read(&journal).unwrap(), initial);
        }
        let applied = maintain_storage(&root, false).unwrap();
        assert_eq!(applied.removed_logical_bytes, expected, "{:#?}", applied.items);
        assert!(!root.join(&representative.worktree).exists());
        assert!(!parent.join(format!("{}.json", members[0].site_id)).exists());
        let ledger = fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap();
        assert_eq!(ledger, events.iter().map(|e| serde_json::to_string(e).unwrap()+"\n").collect::<String>().as_bytes());
        assert_eq!(maintain_storage(&root, false).unwrap().removed_logical_bytes, 0);
    }

    #[test]
    fn equivalent_group_effect_recheck_refuses_changed_identity_and_new_claim_domain() {
        use std::cell::Cell;
        let (root, members, events) = retired_group_fixture();
        let count = Cell::new(0);
        let changed = || {
            count.set(count.get()+1);
            if count.get() == 3 {
                let target = root.join(&members[0].target);
                fs::rename(&target, root.join("preserved-cache")).unwrap();
                fs::create_dir(&target).unwrap();
                fs::write(target.join("new-owner"), b"replacement").unwrap();
            }
            true
        };
        let result = crate::sites::maintain_equivalent_implementations(&root, "rMaint", &events, &members, false, &changed);
        assert!(result.iter().all(|i| i.disposition != "removed" && i.removed_logical_bytes == 0), "{result:#?}");
        assert!(root.join(&members[0].worktree).exists());
        assert!(root.join("preserved-cache/cache").exists());
        let inventory = storage_inventory(&root);
        fs::create_dir(root.join("coordination/rounds/rNewOwner")).unwrap();
        assert!(!maintenance_inventory_unchanged(&root, &inventory));
    }

    fn write_maintenance_events(root: &Path, round: &str, events: &[EventRecord]) {
        let path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect::<String>();
        fs::write(path, bytes).unwrap();
    }

    fn recursive_manifest(root: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime, Vec<u8>)> {
        let mut rows = Vec::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            let m = fs::symlink_metadata(&path).unwrap();
            rows.push((
                path.clone(),
                m.len(),
                m.modified().unwrap(),
                if m.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    Vec::new()
                },
            ));
            if m.is_dir() && !m.file_type().is_symlink() {
                rows.extend(recursive_manifest(&path));
            }
        }
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    #[test]
    fn maintenance_closed_native_inbox_preview_apply_and_replay_preserve_history() {
        let (root, site, _) = maintenance_fixture("maintenance-closed");
        // Stale index stat data makes an accidental ordinary git status observable.
        let tracked = root.join(&site.worktree).join("tracked.txt");
        let bytes = fs::read(&tracked).unwrap();
        fs::write(&tracked, &bytes).unwrap();
        let before = recursive_manifest(&root);
        let preview = maintain_storage(&root, true).unwrap();
        let item = preview
            .items
            .iter()
            .find(|i| i.id == "rMaint/M1-review-probe-g01")
            .unwrap();
        assert!(item.eligible, "{}", item.reason);
        assert_eq!(item.disposition, "held");
        assert_eq!(recursive_manifest(&root), before);
        let ledger = root.join("coordination/rounds/rMaint/events.jsonl");
        let history = fs::read(&ledger).unwrap();
        let expected =
            measure_boundaries(&[root.join(&site.worktree), root.join(&site.target)]).unwrap();
        let applied = maintain_storage(&root, false).unwrap();
        let item = applied
            .items
            .iter()
            .find(|i| i.id == "rMaint/M1-review-probe-g01")
            .unwrap();
        assert_eq!(item.disposition, "removed", "{}", item.reason);
        assert_eq!(applied.removed_logical_bytes, expected);
        assert!(!root.join(&site.target).exists());
        assert_eq!(fs::read(&ledger).unwrap(), history);
        assert!(root
            .join("coordination/runtime/review-inbox/rMaint/M1.md")
            .is_file());
        assert_eq!(
            latest_maintenance_report(&root)
                .unwrap()
                .unwrap()
                .removed_logical_bytes,
            expected
        );
        assert_eq!(
            maintain_storage(&root, false)
                .unwrap()
                .removed_logical_bytes,
            0
        );
        assert_eq!(fs::read(ledger).unwrap(), history);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_rejects_unknown_native_wrong_head_missing_and_changed_output() {
        let (root, site, events) = maintenance_fixture("maintenance-native-refusals");
        let evidence = root.join("coordination/runtime/review-inbox/rMaint/M1.md");
        for mode in 0..5 {
            let mut bad = events.clone();
            match mode {
                0 => bad[1].payload.as_mut().unwrap()["turnEnded"] = serde_json::json!(false),
                1 => {
                    bad[1].payload.as_mut().unwrap()["channelBinding"]["fixedHead"] =
                        serde_json::json!("0".repeat(40))
                }
                2 => {
                    fs::rename(&evidence, evidence.with_extension("saved")).unwrap();
                }
                3 => {
                    fs::write(&evidence, b"changed").unwrap();
                }
                _ => {
                    let outside = root.join("outside-review.md");
                    fs::write(&outside, b"preserved native answer").unwrap();
                    bad[1].payload.as_mut().unwrap()["outputPath"] = serde_json::json!(outside);
                }
            }
            write_maintenance_events(&root, "rMaint", &bad);
            let history = fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap();
            let report = maintain_storage(&root, false).unwrap();
            let item = report
                .items
                .iter()
                .find(|i| i.id == "rMaint/M1-review-probe-g01")
                .unwrap();
            assert_eq!(item.disposition, "held", "mode={mode} {}", item.reason);
            assert!(root.join(&site.target).join("cache").exists());
            assert_eq!(
                fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap(),
                history
            );
            if mode == 2 {
                fs::rename(evidence.with_extension("saved"), &evidence).unwrap();
            }
            fs::write(&evidence, b"preserved native answer").unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_preserves_blocked_overlimit_open_and_symlink_scenes() {
        for mode in 0..4 {
            let (root, site, mut events) =
                maintenance_fixture(&format!("maintenance-physical-{mode}"));
            let target = root.join(&site.target);
            let worktree = root.join(&site.worktree);
            let mut open_file = None;
            match mode {
                0 => {
                    events.push(crate::ledger::event(
                        "AttemptBlocked",
                        "runtime:orch",
                        Some("M1"),
                        Some("rMaint"),
                        serde_json::json!({"attemptId":"M1-A0001"}),
                    ));
                    write_maintenance_events(&root, "rMaint", &events);
                }
                1 => {
                    fs::File::create(worktree.join("large-diagnostic-cache"))
                        .unwrap()
                        .set_len(crate::sites::QUARANTINE_LIMIT_BYTES + 1)
                        .unwrap();
                }
                2 => {
                    open_file = Some(fs::File::open(target.join("cache")).unwrap());
                }
                _ => {
                    let evidence = root.join("coordination/runtime/review-inbox/rMaint/M1.md");
                    let saved = evidence.with_extension("saved");
                    fs::rename(&evidence, &saved).unwrap();
                    std::os::unix::fs::symlink(saved, evidence).unwrap();
                }
            }
            let history = fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap();
            let report = maintain_storage(&root, false).unwrap();
            let item = report
                .items
                .iter()
                .find(|i| i.id == "rMaint/M1-review-probe-g01")
                .unwrap();
            assert_eq!(item.disposition, "held", "mode={mode}: {}", item.reason);
            assert!(target.join("cache").is_file());
            assert!(worktree.join("tracked.txt").is_file());
            assert_eq!(report.removed_logical_bytes, 0);
            assert_eq!(
                fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap(),
                history
            );
            drop(open_file);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_unknown_boundary_does_not_hide_measurable_sibling() {
        let (root, _, _) = maintenance_fixture("maintenance-mixed-measurement");
        let good = root.join("good-boundary");fs::create_dir_all(&good).unwrap();
        fs::write(good.join("payload"), b"known bytes").unwrap();
        let bad = root.join(".bad-boundary");std::os::unix::fs::symlink(&good, &bad).unwrap();
        let item = MaintenanceItem::held("mixed".into(), "site", vec![bad, good], "fixture".into());
        let report = finish_maintenance_report(&root, None, true, vec![item], Vec::new(), vec![root.clone()]);
        assert_eq!(report.items[0].logical_bytes, None);
        assert_eq!(report.measured_logical_bytes, b"known bytes".len() as u64);
        assert_eq!(report.unmeasured_boundaries, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_shared_boundary_sizes_refresh_between_reports() {
        let (root, site, events) = maintenance_fixture("maintenance-size-freshness");
        let mut other = events.clone();
        for event in &mut other { event.round = Some("rOther".into()); }
        write_maintenance_events(&root, "rOther", &other);
        let first = maintain_storage(&root, false).unwrap();
        let first_sites = first.items.iter().filter(|item| item.kind == "site").collect::<Vec<_>>();
        assert_eq!(first_sites.len(), 2);
        assert!(first_sites.iter().all(|item| item.disposition == "held"));
        assert_eq!(first_sites[0].logical_bytes, first_sites[1].logical_bytes);
        let payload = root.join(&site.target).join("cache");
        let old_length = fs::metadata(&payload).unwrap().len();
        fs::write(payload, vec![0u8; 1024]).unwrap();
        let second = maintain_storage(&root, false).unwrap();
        let second_sites = second.items.iter().filter(|item| item.kind == "site").collect::<Vec<_>>();
        assert!(second_sites.iter().all(|item| item.logical_bytes == first_sites[0].logical_bytes.map(|n| n + 1024 - old_length)));
        assert_eq!(second.measured_logical_bytes, first.measured_logical_bytes + 1024 - old_length);
        assert_eq!(second.removed_logical_bytes, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_regular_round_metadata_does_not_block_verified_reclamation() {
        let (root, site, _) = maintenance_fixture("maintenance-round-files");
        for name in [".DS_Store", "README.md", "rNotADirectory"] {
            fs::write(root.join("coordination/rounds").join(name), b"preserved metadata").unwrap();
        }
        let before = recursive_manifest(&root);
        let preview = maintain_storage(&root, true).unwrap();
        assert!(!preview.items.iter().any(|item| item.disposition == "failed"), "{preview:?}");
        assert!(preview.items.iter().any(|item| item.id.ends_with(&site.site_id) && item.eligible));
        assert_eq!(recursive_manifest(&root), before);
        let ledger = root.join("coordination/rounds/rMaint/events.jsonl");
        let history = fs::read(&ledger).unwrap();
        let applied = maintain_storage(&root, false).unwrap();
        assert!(applied.items.iter().any(|item| item.id.ends_with(&site.site_id) && item.disposition == "removed"));
        for name in [".DS_Store", "README.md", "rNotADirectory"] {
            assert_eq!(fs::read(root.join("coordination/rounds").join(name)).unwrap(), b"preserved metadata");
        }
        assert_eq!(fs::read(ledger).unwrap(), history);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_metadata_names_never_exempt_unsafe_directories_or_links() {
        for linked in [false, true] {
            let (root, site, _) = maintenance_fixture("maintenance-round-unsafe-type");
            let path = root.join("coordination/rounds/.DS_Store");
            if linked {
                let target = root.join("round-link-target"); fs::create_dir_all(&target).unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            } else { fs::create_dir_all(&path).unwrap(); }
            let before = recursive_manifest(&root);
            let preview = maintain_storage(&root, true).unwrap();
            assert!(preview.items.iter().any(|item| item.disposition == "failed" && item.reason.contains("unsafe round directory identity")));
            assert_eq!(recursive_manifest(&root), before);
            let applied = maintain_storage(&root, false).unwrap();
            assert_eq!(applied.removed_logical_bytes, 0);
            assert!(root.join(&site.target).join("cache").is_file());
            assert!(fs::symlink_metadata(path).is_ok());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn maintenance_unregistered_inventory_keeps_files_without_calling_them_cache_directories() {
        let (root, _, _) = maintenance_fixture("maintenance-non-cache-files");
        let scratch = root.join(".cowork-temp");fs::create_dir_all(scratch.join("unknown-cache")).unwrap();
        let evidence = scratch.join("proof.json");fs::write(&evidence,b"evidence").unwrap();
        let payload = scratch.join("unknown-cache/payload");fs::write(&payload,b"unregistered cache").unwrap();
        let applied = maintain_storage(&root,false).unwrap();
        assert!(!applied.items.iter().any(|item| item.paths.contains(&evidence)));
        assert!(applied.items.iter().any(|item| item.kind=="unregistered" && item.paths.contains(&scratch.join("unknown-cache")) && item.disposition=="held"));
        assert_eq!(fs::read(evidence).unwrap(),b"evidence");
        assert_eq!(fs::read(payload).unwrap(),b"unregistered cache");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_legacy_open_is_held_but_closed_generation_can_reclaim() {
        let (root,site,mut events)=maintenance_fixture("maintenance-legacy-boundary");
        events.retain(|event| event.kind != "RoundClosed");
        events.insert(0,crate::ledger::event("RoundOpened","runtime:orch",None,Some("rMaint"),serde_json::json!({"contractSchemaVersion":2})));
        write_maintenance_events(&root,"rMaint",&events);
        let history=fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap();
        let held=maintain_storage(&root,false).unwrap();
        assert!(held.items.iter().any(|item| item.disposition=="failed" && item.reason.contains("schema 1/2 active round")));
        assert!(root.join(&site.target).join("cache").is_file());
        assert_eq!(fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap(),history);
        events.push(crate::ledger::event("RoundClosed","runtime:orch",None,Some("rMaint"),serde_json::json!({"forced":false})));
        write_maintenance_events(&root,"rMaint",&events);
        let closed=fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap();
        let report=maintain_storage(&root,false).unwrap();
        assert!(report.items.iter().any(|item| item.id.ends_with(&site.site_id) && item.disposition=="removed"));
        assert_eq!(fs::read(root.join("coordination/rounds/rMaint/events.jsonl")).unwrap(),closed);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_cross_round_claim_and_bounded_bad_round_are_isolated() {
        let (root, site, events) = maintenance_fixture("maintenance-round-isolation");
        let mut conflicting = events.clone();
        for event in &mut conflicting {
            event.round = Some("rOther".into());
        }
        write_maintenance_events(&root, "rOther", &conflicting);
        let held = maintain_storage(&root, false).unwrap();
        assert!(held
            .items
            .iter()
            .filter(|i| i.kind == "site")
            .all(|i| i.disposition == "held"));
        assert!(root.join(&site.target).exists());
        // A malformed but bounded unrelated lease cannot hide its claim or veto unrelated paths.
        let mut bad = events[0].clone();
        bad.round = Some("rOther".into());
        let payload = bad.payload.as_mut().unwrap();
        payload["role"] = serde_json::json!("illegal");
        payload["paths"] =
            serde_json::json!({"worktree":".worktrees/unrelated","target":"orch/target/unrelated"});
        write_maintenance_events(&root, "rOther", &[bad]);
        let history = fs::read(root.join("coordination/rounds/rOther/events.jsonl")).unwrap();
        let report = maintain_storage(&root, false).unwrap();
        assert_eq!(
            report
                .items
                .iter()
                .find(|i| i.id == "rMaint/M1-review-probe-g01")
                .unwrap()
                .disposition,
            "removed"
        );
        assert_eq!(
            fs::read(root.join("coordination/rounds/rOther/events.jsonl")).unwrap(),
            history
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_pending_wal_intent_and_dry_run_never_repair_history() {
        let (root, site, _) = maintenance_fixture("maintenance-wal-intent");
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/.rMaint.atomic-batch-intent.json"),
            b"unresolved fixture intent",
        )
        .unwrap();
        let before = recursive_manifest(&root);
        let preview = maintain_storage(&root, true).unwrap();
        assert_eq!(before, recursive_manifest(&root));
        assert!(preview.items.iter().any(|i| i.reason.contains("intent")));
        let report = maintain_storage(&root, false).unwrap();
        assert!(report
            .items
            .iter()
            .filter(|i| i.kind == "site")
            .all(|i| i.disposition == "held"));
        assert!(root.join(&site.target).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_nested_locks_and_report_failure_do_not_erase_outcomes() {
        let (root, site, _) = maintenance_fixture("maintenance-locks");
        crate::close::with_protocol_effect(&root, "fixture shared", || {
            let report = maintain_storage(&root, true)?;
            assert!(report.items.iter().any(|i| i.eligible));
            Ok(())
        })
        .unwrap();
        fs::create_dir_all(root.join(MAINTENANCE_REPORT)).unwrap(); // deliberate report-file collision
        let report = crate::close::with_protocol_transition(&root, "fixture exclusive", || {
            maintain_storage(&root, false)
        })
        .unwrap();
        assert!(!root.join(&site.target).exists());
        assert!(report.removed_logical_bytes > 0);
        assert!(report.report_error.is_some());
        fs::remove_dir_all(root).unwrap();
    }

    fn task_record() -> orch_core::EventRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": "EV-RECORDED",
            "ts": "2026-08-07T00:00:00Z",
            "actor": "runtime:orch",
            "type": "TaskRecorded",
            "round": "r66",
            "taskId": "B237",
            "payload": { "postMergeGates": "all-green" }
        }))
        .unwrap()
    }

    #[test]
    fn only_canonical_task_records_authorize_reclamation() {
        let canonical = task_record();
        assert!(task_record_is_canonical(&canonical, "r66"));

        let mut forged_actor = canonical.clone();
        forged_actor.actor = "executor:desktop".to_string();
        assert!(!task_record_is_canonical(&forged_actor, "r66"));

        let mut wrong_round = canonical.clone();
        wrong_round.round = Some("r65".to_string());
        assert!(!task_record_is_canonical(&wrong_round, "r66"));

        let mut red = canonical;
        red.payload = Some(serde_json::json!({ "postMergeGates": "red" }));
        assert!(!task_record_is_canonical(&red, "r66"));

        let mut extra_payload = task_record();
        extra_payload.payload = Some(serde_json::json!({
            "postMergeGates": "all-green",
            "forged": true
        }));
        assert!(!task_record_is_canonical(&extra_payload, "r66"));

        let mut extra_top_level = task_record();
        extra_top_level
            .extra
            .insert("forged".to_string(), serde_json::Value::Bool(true));
        assert!(!task_record_is_canonical(&extra_top_level, "r66"));
    }

    #[test]
    fn active_implement_lease_blocks_the_task_site_reclaimer() {
        let root = crate::util::test_scratch_dir("reclaim-active-implement-lease");
        git_test(&root, &["init", "-b", "main"]);
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\ncoordination/runtime/\n",
        )
        .unwrap();
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git_test(&root, &["add", ".gitignore", "tracked.txt"]);
        git_test(&root, &["commit", "-m", "base"]);
        let base = git_test(&root, &["rev-parse", "HEAD"]);
        let worktree = root.join(".worktrees/B238");
        fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        crate::gitx::worktree_add(&root, &worktree, "task/B238", &base).unwrap();
        fs::write(worktree.join("tracked.txt"), "task\n").unwrap();
        git_test(&worktree, &["add", "tracked.txt"]);
        git_test(&worktree, &["commit", "-m", "task"]);
        git_test(
            &root,
            &["merge", "--no-ff", "task/B238", "-m", "merge task"],
        );

        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r67\n").unwrap();
        let lease = crate::ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("B238"),
            Some("r67"),
            serde_json::json!({
                "siteId": "B238-implement-executor-desktop-g01",
                "generation": 1,
                "attemptId": "B238-A0001",
                "role": "implement",
                "agent": "executor-desktop",
                "reviewedHead": base,
                "paths": {
                    "worktree": ".worktrees/B238",
                    "target": ".worktrees/B238/orch/target",
                },
            }),
        );
        let recorded_fact = crate::ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B238"),
            Some("r67"),
            serde_json::json!({"postMergeGates": "all-green"}),
        );
        crate::ledger::append(&root, "r67", &[lease, recorded_fact]).unwrap();

        let recorded = tasks_recorded_in_any_round(&root).unwrap();
        let criteria = observe_task_site_criteria(&root, &worktree, "B238", &recorded);
        assert_eq!(
            decide_task_site_reclaim(&criteria),
            TaskSiteDisposition::Reclaim,
            "the lease guard, not a hidden failed criterion, must be the only blocker"
        );

        let report = reclaim_task_sites(&root).unwrap();
        let [verdict] = report.verdicts.as_slice() else {
            panic!("expected one task-site verdict: {:?}", report.verdicts);
        };
        assert_eq!(verdict.task_id, "B238");
        assert_eq!(
            verdict.disposition.keep_reason(),
            Some("active lease B238-implement-executor-desktop-g01")
        );
        assert!(worktree.exists(), "active task site must remain intact");
        assert!(crate::gitx::branch_exists(&root, "task/B238"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_noncanonical_task_record_skips_the_whole_round() {
        let root = crate::util::test_scratch_dir("reclaim-noncanonical-record-round");
        let ledger_path = root.join("coordination/rounds/r66/events.jsonl");
        fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
        let canonical = task_record();
        let mut forged = task_record();
        forged.event_id = "EV-FORGED-RECORDED".to_string();
        forged.task_id = Some("B999".to_string());
        forged.actor = "executor:desktop".to_string();
        let bytes = format!(
            "{}\n{}\n",
            serde_json::to_string(&canonical).unwrap(),
            serde_json::to_string(&forged).unwrap()
        );
        fs::write(ledger_path, bytes).unwrap();

        let recorded = tasks_recorded_in_any_round(&root).unwrap();
        assert!(recorded.by_task.is_empty());
        assert_eq!(recorded.skipped_rounds, vec!["r66"]);
    }

    #[test]
    fn residual_governance_requires_concrete_report_evidence() {
        let empty = BTreeSet::new();
        assert_eq!(
            residual_governance(".worktrees/orphan", &empty, &empty, &empty),
            Governance::Ungoverned
        );
        assert_eq!(
            residual_governance("orch/target/review-orphan", &empty, &empty, &empty),
            Governance::Ungoverned
        );

        let governed = BTreeSet::from(["orch/target/review-orphan".to_string()]);
        assert_eq!(
            residual_governance("orch/target/review-orphan", &empty, &empty, &governed),
            Governance::GovernedResidual
        );
    }

    #[cfg(unix)]
    #[test]
    fn disk_survey_rejects_non_utf8_child_names_instead_of_colliding() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let child = OsString::from_vec(vec![b'x', 0xff]);
        assert!(disk_survey_child_name(&child).is_err());
    }
}
