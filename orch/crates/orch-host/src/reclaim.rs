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
    match gitx::porcelain_v2(worktree) {
        Ok(status) if status.trim().is_empty() => Criterion::Met,
        Ok(_) => Criterion::Unmet,
        Err(_) => Criterion::Indeterminate,
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
