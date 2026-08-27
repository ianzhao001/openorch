//! Conservative runtime-log rotation.
//!
//! The ledger is the authority: a round contributes archive candidates only
//! after its own `RoundClosed` fact exists.  Open-round task prefixes and
//! `WakeIssued.logPath` values are explicit preserves, and the legacy
//! `wake-<agent>.log` handshake relink is never a candidate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use orch_core::EventRecord;

static ARCHIVE_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveMapping {
    pub round: String,
    pub source: PathBuf,
    pub archive: PathBuf,
    pub source_bytes: u64,
    pub archive_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttributionConflict {
    pub source: PathBuf,
    pub rounds: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotateFailure {
    pub round: String,
    pub source: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotateReport {
    pub archived: Vec<ArchiveMapping>,
    pub orphaned: Vec<ArchiveMapping>,
    pub skipped_bad_ledgers: Vec<String>,
    pub conflicts: Vec<AttributionConflict>,
    pub failures: Vec<RotateFailure>,
}

#[derive(Debug)]
pub struct RotateUnresolved {
    pub report: RotateReport,
}

impl fmt::Display for RotateUnresolved {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unresolved = self.report.conflicts.len() + self.report.failures.len();
        writeln!(
            formatter,
            "日志轮转已完成（已归档 {} 项、孤儿 {} 项），但有 {} 项需人工处置：",
            self.report.archived.len(),
            self.report.orphaned.len(),
            unresolved
        )?;
        for conflict in &self.report.conflicts {
            writeln!(
                formatter,
                "  CONFLICT {}（同时被已收轮 {} 认领，拒绝归属）",
                conflict.source.display(),
                conflict.rounds.join("/")
            )?;
        }
        for failure in &self.report.failures {
            writeln!(
                formatter,
                "  FAILED {} {}: {}",
                failure.round,
                failure.source.display(),
                failure.reason
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for RotateUnresolved {}

fn payload_string<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
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

fn legacy_handshake_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("wake-") && name.ends_with(".log"))
}

fn event_is_in_round(event: &EventRecord, round: &str) -> bool {
    event.round.as_deref() == Some(round)
}

fn round_is_closed(events: &[EventRecord], round: &str) -> bool {
    events
        .iter()
        .any(|event| event_is_in_round(event, round) && event.kind == "RoundClosed")
}

fn task_ids_for_round(events: &[EventRecord], round: &str) -> BTreeSet<String> {
    events
        .iter()
        .filter(|event| event_is_in_round(event, round))
        .filter_map(|event| event.task_id.as_deref())
        .filter(|task_id| safe_component(task_id))
        .map(str::to_string)
        .collect()
}

/// Pure deterministic mapping seed for one round.
///
/// The returned paths include exact `WakeIssued.logPath` references and the
/// stable gate/verify filenames derived from that round's task identities.
/// Filesystem discovery of additional task-owned `.log`, `.jsonl`, and `.hb`
/// variants happens only in [`rotate_closed_round_logs`].
pub fn archive_set_for_round(events: &[EventRecord], round: &str) -> Vec<PathBuf> {
    if !round_is_closed(events, round) {
        return Vec::new();
    }
    let mut paths = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event_is_in_round(event, round) && event.kind == "WakeIssued")
    {
        let Some(path) = payload_string(event, "logPath").map(PathBuf::from) else {
            continue;
        };
        if !legacy_handshake_name(&path) {
            paths.insert(path);
        }
    }
    for task_id in task_ids_for_round(events, round) {
        let base = Path::new("coordination/runtime/logs");
        let scoped_tag = crate::gate::round_scoped_log_tag(round, &task_id);
        for gate in ["testFast", "check"] {
            for extension in ["log", "hb", "orphans", "fixtures"] {
                paths.insert(base.join(format!("{scoped_tag}-gate-{gate}.{extension}")));
            }
        }
        paths.insert(base.join(format!("{task_id}-verify.jsonl")));
    }
    paths.into_iter().collect()
}

fn task_tag_name(tag: &str, task_id: &str) -> bool {
    tag == task_id
        || tag.starts_with(&format!("{task_id}-"))
        || tag == format!("_oracle-{task_id}")
        || tag.starts_with(&format!("_oracle-{task_id}-"))
        || tag == format!("review-{task_id}")
        || tag.starts_with(&format!("review-{task_id}-"))
}

fn scoped_gate_identity(name: &str) -> Option<(&str, &str)> {
    let stem = [".log", ".hb", ".orphans", ".fixtures"]
        .into_iter()
        .find_map(|extension| name.strip_suffix(extension))?;
    let (tag_and_round, gate_name) = stem.rsplit_once("-gate-")?;
    if gate_name.is_empty() {
        return None;
    }
    let (tag, round) = tag_and_round.rsplit_once("-round-")?;
    (!tag.is_empty() && safe_component(round)).then_some((tag, round))
}

fn task_log_name(name: &str, task_id: &str) -> bool {
    // Canonical round-scoped gate artifacts are attributed by their parsed exact round before the
    // broad legacy task-prefix matcher runs.  Falling through would recreate the cross-round
    // conflict this namespace is designed to prevent.
    if scoped_gate_identity(name).is_some() {
        return false;
    }
    let direct = name.starts_with(&format!("{task_id}-"));
    let oracle = name.starts_with(&format!("_oracle-{task_id}-"));
    let review = name.starts_with(&format!("review-{task_id}-"));
    (direct || oracle || review)
        && (name.ends_with(".log")
            || name.ends_with(".jsonl")
            || name.ends_with(".hb")
            || name.ends_with(".orphans")
            || name.ends_with(".fixtures"))
}

fn direct_runtime_log(log_dir: &Path, candidate: &Path) -> Option<PathBuf> {
    let candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        log_dir.parent()?.parent()?.parent()?.join(candidate)
    };
    (candidate.parent() == Some(log_dir)).then_some(candidate)
}

/// Deterministic archive location for a ledger reference.  Consumers can use
/// this after the original path disappears, so append-only ledger references
/// remain resolvable without rewriting historical events.
pub fn archive_path_for_round(root: &Path, round: &str, source: &Path) -> Option<PathBuf> {
    if !safe_component(round) || legacy_handshake_name(source) {
        return None;
    }
    let name = source.file_name()?;
    Some(
        root.join("coordination/runtime/logs/archive")
            .join(round)
            .join(format!("{}.gz", name.to_string_lossy())),
    )
}

pub fn resolve_log_reference(root: &Path, round: &str, source: &Path) -> Option<PathBuf> {
    let source = if source.is_absolute() {
        source.to_path_buf()
    } else {
        root.join(source)
    };
    if source.is_file() {
        return Some(source);
    }
    archive_path_for_round(root, round, &source).filter(|path| path.is_file())
}

fn round_opened_at(events: &[EventRecord], round: &str) -> Option<SystemTime> {
    events
        .iter()
        .filter(|event| event_is_in_round(event, round) && event.kind == "RoundOpened")
        .filter_map(|event| humantime::parse_rfc3339(&event.ts).ok())
        .min()
}

fn compress_one(root: &Path, round: &str, source: &Path) -> Result<ArchiveMapping> {
    let archive =
        archive_path_for_round(root, round, source).context("日志归档目标不是安全的确定性路径")?;
    if archive.exists() {
        bail!(
            "日志归档目标已存在而源仍在，拒绝覆盖: {}",
            archive.display()
        );
    }
    let parent = archive.parent().context("日志归档目标缺父目录")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("创建日志归档目录失败: {}", parent.display()))?;
    let source_bytes = fs::metadata(source)
        .with_context(|| format!("读取日志大小失败: {}", source.display()))?
        .len();
    let seq = ARCHIVE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{seq}",
        archive
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("archive.gz"),
        std::process::id()
    ));
    let output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("创建日志归档临时文件失败: {}", temporary.display()))?;
    let result = Command::new("gzip")
        .args(["-c", "--"])
        .arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .output()
        .with_context(|| "启动 gzip 失败（日志原文件未删除）")?;
    if !result.status.success() {
        let _ = fs::remove_file(&temporary);
        bail!(
            "gzip 失败({}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    File::open(&temporary)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("sync 日志归档失败: {}", temporary.display()))?;
    fs::rename(&temporary, &archive)
        .with_context(|| format!("提交日志归档失败: {}", archive.display()))?;
    let archive_bytes = fs::metadata(&archive)
        .with_context(|| format!("读取日志归档大小失败: {}", archive.display()))?
        .len();
    fs::remove_file(source).with_context(|| {
        format!(
            "归档已写但删除源日志失败（可安全人工复核）: {}",
            source.display()
        )
    })?;
    Ok(ArchiveMapping {
        round: round.to_string(),
        source: source.to_path_buf(),
        archive,
        source_bytes,
        archive_bytes,
    })
}

fn insert_candidate(
    candidates: &mut BTreeMap<PathBuf, String>,
    conflicts: &mut BTreeMap<PathBuf, BTreeSet<String>>,
    source: PathBuf,
    round: &str,
) {
    if let Some(rounds) = conflicts.get_mut(&source) {
        rounds.insert(round.to_string());
        return;
    }
    match candidates.get(&source).cloned() {
        Some(existing) if existing != round => {
            candidates.remove(&source);
            conflicts
                .entry(source)
                .or_default()
                .extend([existing, round.to_string()]);
        }
        Some(_) => {}
        None => {
            candidates.insert(source, round.to_string());
        }
    }
}

fn discovered_regular_file(disk_files: &[PathBuf], source: &PathBuf) -> bool {
    disk_files.binary_search(source).is_ok()
        && fs::symlink_metadata(source).is_ok_and(|metadata| metadata.file_type().is_file())
}

/// Archive every safely attributable closed-round log beneath the runtime log
/// directory.  Bad ledgers, ambiguous ownership, open-round files, symlinks,
/// and legacy handshake relinks all fail closed and remain untouched.
pub fn rotate_closed_round_logs(root: &Path) -> Result<RotateReport> {
    let log_dir = root.join("coordination/runtime/logs");
    if !log_dir.exists() {
        return Ok(RotateReport::default());
    }
    let current_round = crate::current_round(root).ok();
    let rounds_dir = root.join("coordination/rounds");
    let mut ledgers = Vec::<(String, Vec<EventRecord>)>::new();
    let mut report = RotateReport::default();
    for entry in fs::read_dir(&rounds_dir)
        .with_context(|| format!("读取轮目录失败: {}", rounds_dir.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let round = entry.file_name().to_string_lossy().to_string();
        if !safe_component(&round) {
            continue;
        }
        let ledger_path = entry.path().join("events.jsonl");
        if !ledger_path.is_file() {
            continue;
        }
        let read = orch_core::read_ledger(&ledger_path)
            .with_context(|| format!("读取轮账本失败: {}", ledger_path.display()))?;
        if !read.bad_lines.is_empty() {
            report.skipped_bad_ledgers.push(round);
            continue;
        }
        ledgers.push((round, read.events));
    }
    ledgers.sort_by(|left, right| left.0.cmp(&right.0));

    let mut candidates = BTreeMap::<PathBuf, String>::new();
    let mut conflicts = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    let mut protected = BTreeSet::<PathBuf>::new();
    let mut referenced = BTreeSet::<PathBuf>::new();
    let mut closed_tasks = Vec::<(String, BTreeSet<String>)>::new();
    let mut open_round_tasks = Vec::<(String, BTreeSet<String>)>::new();
    let mut open_tasks = BTreeSet::<String>::new();
    let mut current_opened_at = None;

    for (round, events) in &ledgers {
        let closed = round_is_closed(events, round);
        let tasks = task_ids_for_round(events, round);
        if current_round.as_deref() == Some(round.as_str()) {
            current_opened_at = round_opened_at(events, round);
        }
        for event in events
            .iter()
            .filter(|event| event_is_in_round(event, round) && event.kind == "WakeIssued")
        {
            let Some(raw) = payload_string(event, "logPath") else {
                continue;
            };
            let Some(path) = direct_runtime_log(&log_dir, Path::new(raw)) else {
                continue;
            };
            referenced.insert(path.clone());
            if legacy_handshake_name(&path) {
                protected.insert(path);
            } else if closed {
                insert_candidate(&mut candidates, &mut conflicts, path, round);
            } else {
                protected.insert(path);
            }
        }
        if closed {
            closed_tasks.push((round.clone(), tasks));
        } else {
            open_tasks.extend(tasks.iter().cloned());
            open_round_tasks.push((round.clone(), tasks));
        }
    }

    let mut disk_files = Vec::new();
    for entry in fs::read_dir(&log_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if legacy_handshake_name(&path) {
            protected.insert(path.clone());
        }
        disk_files.push(path);
    }
    disk_files.sort();

    for path in &disk_files {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some((tag, scoped_round)) = scoped_gate_identity(name) {
            let mut attributed = false;
            for (round, tasks) in &open_round_tasks {
                if round == scoped_round && tasks.iter().any(|task| task_tag_name(tag, task)) {
                    protected.insert(path.clone());
                    candidates.remove(path);
                    attributed = true;
                }
            }
            for (round, tasks) in &closed_tasks {
                if round == scoped_round && tasks.iter().any(|task| task_tag_name(tag, task)) {
                    insert_candidate(&mut candidates, &mut conflicts, path.clone(), round);
                    attributed = true;
                }
            }
            if !attributed {
                // A syntactically scoped file whose exact round/task cannot be proved from a good
                // ledger is evidence, not an orphan eligible for age-based reassignment.
                protected.insert(path.clone());
                candidates.remove(path);
            }
            continue;
        }
        if open_tasks.iter().any(|task| task_log_name(name, task)) {
            protected.insert(path.clone());
            candidates.remove(path);
            continue;
        }
        for (round, tasks) in &closed_tasks {
            if tasks.iter().any(|task| task_log_name(name, task)) {
                insert_candidate(&mut candidates, &mut conflicts, path.clone(), round);
            }
        }
    }
    for (source, rounds) in conflicts {
        if !discovered_regular_file(&disk_files, &source) {
            continue;
        }
        protected.insert(source.clone());
        report.conflicts.push(AttributionConflict {
            source,
            rounds: rounds.into_iter().collect(),
        });
    }

    for (source, round) in candidates {
        if protected.contains(&source) || !discovered_regular_file(&disk_files, &source) {
            continue;
        }
        match compress_one(root, &round, &source) {
            Ok(mapping) => report.archived.push(mapping),
            Err(error) => {
                protected.insert(source.clone());
                report.failures.push(RotateFailure {
                    round,
                    source,
                    reason: format!("{error:#}"),
                });
            }
        }
    }

    if let (Some(current), Some(opened_at)) = (current_round.as_deref(), current_opened_at) {
        let orphan_round = format!("orphans-before-{current}");
        for source in disk_files {
            if !source.is_file()
                || protected.contains(&source)
                || referenced.contains(&source)
                || legacy_handshake_name(&source)
            {
                continue;
            }
            let modified = match fs::metadata(&source).and_then(|meta| meta.modified()) {
                Ok(modified) => modified,
                Err(_) => continue,
            };
            if modified >= opened_at {
                continue;
            }
            match compress_one(root, &orphan_round, &source) {
                Ok(mapping) => report.orphaned.push(mapping),
                Err(error) => {
                    protected.insert(source.clone());
                    report.failures.push(RotateFailure {
                        round: orphan_round.clone(),
                        source,
                        reason: format!("{error:#}"),
                    });
                }
            }
        }
    }
    report
        .archived
        .sort_by(|left, right| left.source.cmp(&right.source));
    report
        .orphaned
        .sort_by(|left, right| left.source.cmp(&right.source));
    report.skipped_bad_ledgers.sort();
    report
        .conflicts
        .sort_by(|left, right| left.source.cmp(&right.source));
    report
        .failures
        .sort_by(|left, right| left.source.cmp(&right.source));
    if report.conflicts.is_empty() && report.failures.is_empty() {
        Ok(report)
    } else {
        Err(anyhow::Error::new(RotateUnresolved { report }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        kind: &str,
        round: &str,
        task: Option<&str>,
        payload: serde_json::Value,
    ) -> EventRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": format!("EV-{kind}-{round}-{}", task.unwrap_or("round")),
            "ts": "2026-08-04T00:00:00Z",
            "actor": "runtime:orch",
            "type": kind,
            "round": round,
            "taskId": task,
            "payload": payload,
        }))
        .unwrap()
    }

    #[test]
    fn deterministic_set_includes_task_files_only_after_close() {
        let events = vec![
            event("DispatchIssued", "r9", Some("B9"), serde_json::json!({})),
            event("RoundClosed", "r9", None, serde_json::json!({})),
        ];
        let set = archive_set_for_round(&events, "r9");
        assert_eq!(set, archive_set_for_round(&events, "r9"));
        assert!(set.iter().any(|path| path.ends_with("B9-verify.jsonl")));
        assert!(set
            .iter()
            .any(|path| path.ends_with("B9-round-r9-gate-testFast.log")));
        assert!(set
            .iter()
            .any(|path| path.ends_with("B9-round-r9-gate-check.fixtures")));
        assert!(archive_set_for_round(&events, "r10").is_empty());
    }

    #[test]
    fn reference_resolver_uses_deterministic_gzip_location() {
        let root = Path::new("/repo");
        let source = Path::new("/repo/coordination/runtime/logs/B9-verify.jsonl");
        assert_eq!(
            archive_path_for_round(root, "r9", source),
            Some(PathBuf::from(
                "/repo/coordination/runtime/logs/archive/r9/B9-verify.jsonl.gz"
            ))
        );
        assert!(archive_path_for_round(root, "r9", Path::new("wake-agent.log")).is_none());
    }

    #[test]
    fn attribution_conflict_retains_every_claiming_round() {
        fn assert_error_traits<T: std::error::Error + Send + Sync + 'static>() {}
        assert_error_traits::<RotateUnresolved>();

        let source = PathBuf::from("/repo/coordination/runtime/logs/B9-gate-check.log");
        let mut candidates = BTreeMap::new();
        let mut conflicts = BTreeMap::new();
        for round in ["r9", "r10", "r11", "r10"] {
            insert_candidate(&mut candidates, &mut conflicts, source.clone(), round);
        }

        assert!(!candidates.contains_key(&source));
        assert_eq!(
            conflicts.get(&source),
            Some(&BTreeSet::from([
                "r10".to_string(),
                "r11".to_string(),
                "r9".to_string(),
            ]))
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_and_symlinked_wake_references_remain_non_candidates() {
        use std::os::unix::fs::symlink;

        let root = crate::util::test_scratch_dir("logrotate-non-regular-reference");
        let logs = root.join("coordination/runtime/logs");
        fs::create_dir_all(&logs).unwrap();
        let missing = logs.join("provider-missing.jsonl");
        let linked = logs.join("provider-linked.jsonl");
        let target = root.join("provider-target.jsonl");
        fs::write(&target, "target\n").unwrap();
        symlink(&target, &linked).unwrap();

        let r1 = vec![
            event("RoundOpened", "r1", None, serde_json::json!({})),
            event(
                "WakeIssued",
                "r1",
                Some("B1"),
                serde_json::json!({"logPath": linked}),
            ),
            event(
                "WakeIssued",
                "r1",
                Some("B2"),
                serde_json::json!({"logPath": missing}),
            ),
            event("RoundClosed", "r1", None, serde_json::json!({})),
        ];
        let r2 = vec![
            event("RoundOpened", "r2", None, serde_json::json!({})),
            event(
                "WakeIssued",
                "r2",
                Some("B3"),
                serde_json::json!({"logPath": missing}),
            ),
            event("RoundClosed", "r2", None, serde_json::json!({})),
        ];
        for (round, events) in [("r1", r1), ("r2", r2)] {
            let dir = root.join(format!("coordination/rounds/{round}"));
            fs::create_dir_all(&dir).unwrap();
            let bytes = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(dir.join("events.jsonl"), format!("{bytes}\n")).unwrap();
        }

        let report = rotate_closed_round_logs(&root).unwrap();
        assert!(report.archived.is_empty());
        assert!(report.conflicts.is_empty());
        assert!(report.failures.is_empty());
        assert!(fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.is_file());
        assert!(!archive_path_for_round(&root, "r1", &linked)
            .unwrap()
            .exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rotation_moves_only_closed_round_files_and_keeps_references_resolvable() {
        let root = crate::util::test_scratch_dir("logrotate-real");
        let logs = root.join("coordination/runtime/logs");
        fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r2")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(&logs).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r2\n").unwrap();

        let closed_wake = logs.join("wake-agent-111-1-1.jsonl");
        let open_wake = logs.join("wake-agent-222-2-2.jsonl");
        let legacy = logs.join("wake-agent.log");
        fs::write(&closed_wake, "closed wake\n").unwrap();
        fs::write(logs.join("B1-gate-testFast.log"), "closed gate\n").unwrap();
        fs::write(&open_wake, "open wake\n").unwrap();
        fs::write(logs.join("B2-gate-testFast.log"), "open gate\n").unwrap();
        fs::write(&legacy, "handshake\n").unwrap();

        let r1 = vec![
            event("RoundOpened", "r1", None, serde_json::json!({})),
            event("DispatchIssued", "r1", Some("B1"), serde_json::json!({})),
            event(
                "WakeIssued",
                "r1",
                Some("B1"),
                serde_json::json!({"logPath": closed_wake}),
            ),
            event("RoundClosed", "r1", None, serde_json::json!({})),
        ];
        let r2 = vec![
            event("RoundOpened", "r2", None, serde_json::json!({})),
            event("DispatchIssued", "r2", Some("B2"), serde_json::json!({})),
            event(
                "WakeIssued",
                "r2",
                Some("B2"),
                serde_json::json!({"logPath": open_wake}),
            ),
        ];
        for (round, events) in [("r1", r1), ("r2", r2)] {
            let bytes = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(
                root.join(format!("coordination/rounds/{round}/events.jsonl")),
                format!("{bytes}\n"),
            )
            .unwrap();
        }

        let outcome = rotate_closed_round_logs(&root).unwrap();
        assert_eq!(outcome.archived.len(), 2);
        assert!(!closed_wake.exists());
        assert!(open_wake.exists());
        assert!(logs.join("B2-gate-testFast.log").exists());
        assert!(legacy.exists());
        let resolved = resolve_log_reference(&root, "r1", &closed_wake).unwrap();
        assert!(resolved.ends_with("archive/r1/wake-agent-111-1-1.jsonl.gz"));
        assert!(Command::new("gzip")
            .args(["-t", "--"])
            .arg(&resolved)
            .status()
            .unwrap()
            .success());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn heartbeat_sidecar_rotates_with_its_gate_log_sibling() {
        let root = crate::util::test_scratch_dir("logrotate-gate-heartbeat");
        let logs = root.join("coordination/runtime/logs");
        let round_dir = root.join("coordination/rounds/r1");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&round_dir).unwrap();

        let events = [
            event("RoundOpened", "r1", None, serde_json::json!({})),
            event("DispatchIssued", "r1", Some("B1"), serde_json::json!({})),
            event("RoundClosed", "r1", None, serde_json::json!({})),
        ];
        let ledger = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(round_dir.join("events.jsonl"), format!("{ledger}\n")).unwrap();

        let gate_log = logs.join("B1-A0001-gate-testFast.log");
        let heartbeat = logs.join("B1-A0001-gate-testFast.hb");
        fs::write(&gate_log, "gate output\n").unwrap();
        fs::write(&heartbeat, "[gate-hb] t=+15s child=42 state=? lastline=1\n").unwrap();

        let report = rotate_closed_round_logs(&root).unwrap();
        assert_eq!(report.archived.len(), 2);
        let gate_archive = archive_path_for_round(&root, "r1", &gate_log).unwrap();
        let heartbeat_archive = archive_path_for_round(&root, "r1", &heartbeat).unwrap();
        assert!(gate_archive.is_file());
        assert!(heartbeat_archive.is_file());
        assert_eq!(gate_archive.parent(), heartbeat_archive.parent());
        assert!(!gate_log.exists());
        assert!(!heartbeat.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scoped_gate_artifacts_claim_only_their_exact_round_while_legacy_conflicts() {
        let root = crate::util::test_scratch_dir("b272-logrotate-round-scope");
        let logs = root.join("coordination/runtime/logs");
        fs::create_dir_all(&logs).unwrap();
        for round in ["r9", "r10"] {
            let round_dir = root.join(format!("coordination/rounds/{round}"));
            fs::create_dir_all(&round_dir).unwrap();
            let events = [
                event("RoundOpened", round, None, serde_json::json!({})),
                event("DispatchIssued", round, Some("B9"), serde_json::json!({})),
                event("RoundClosed", round, None, serde_json::json!({})),
            ];
            let ledger = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(round_dir.join("events.jsonl"), format!("{ledger}\n")).unwrap();
        }

        for extension in ["log", "hb", "orphans", "fixtures"] {
            fs::write(
                logs.join(format!("B9-trial-round-r9-gate-testFast.{extension}")),
                format!("r9 {extension}\n"),
            )
            .unwrap();
        }
        let r10_log = logs.join("B9-trial-round-r10-gate-testFast.log");
        fs::write(&r10_log, "r10 log\n").unwrap();
        let legacy = logs.join("B9-trial-gate-testFast.log");
        fs::write(&legacy, "legacy ambiguous\n").unwrap();

        let error = rotate_closed_round_logs(&root).unwrap_err();
        let unresolved = error.downcast_ref::<RotateUnresolved>().unwrap();
        assert_eq!(unresolved.report.conflicts.len(), 1);
        assert_eq!(unresolved.report.conflicts[0].source, legacy);
        assert_eq!(
            unresolved.report.conflicts[0].rounds,
            vec!["r10".to_string(), "r9".to_string()]
        );
        assert_eq!(unresolved.report.archived.len(), 5);
        for mapping in &unresolved.report.archived {
            let name = mapping.source.file_name().unwrap().to_string_lossy();
            if name.contains("-round-r9-") {
                assert_eq!(mapping.round, "r9");
            } else if name.contains("-round-r10-") {
                assert_eq!(mapping.round, "r10");
            } else {
                panic!("unexpected scoped archive source: {name}");
            }
        }
        assert!(
            legacy.is_file(),
            "ambiguous legacy evidence must remain in place"
        );
    }
}
