//! 门执行器（design/06 §3 机检门 · argv 直跑无 shell，design/08 §1）。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use wait_timeout::ChildExt;

use crate::{binding::CommandSpec, buildcache, cas};

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
    fn getuid() -> u32;
}

pub const ORCH_GATE_FIXTURE_REGISTRY: &str = "ORCH_GATE_FIXTURE_REGISTRY";

const UNKNOWN_BIRTH_IDENTITY: &str = "-";

/// Executable names which belong to a gate's process working set. A trailing `*` means prefix
/// matching so libtest hashes and kernel-truncated fixture names remain recognizable.
pub const GATE_ORPHAN_WORKSET: &[&str] = &[
    "orch_host-*",
    "cargo",
    "rustc",
    "sh",
    // macOS PROC_PIDTBSDINFO reports `/bin/sh` by its backing executable name.
    "bash",
    "sleep",
    "opencode",
    "codex",
    "orch",
    "b143-unattended-sample*",
    "b147-production-depwave*",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOrphan {
    pub pid: u32,
    pub executable: String,
    uid: u32,
    birth_identity: String,
}

impl GateOrphan {
    pub fn new(pid: u32, executable: impl Into<String>) -> Self {
        Self {
            pid,
            executable: executable.into(),
            // SAFETY: getuid has no preconditions and does not mutate process state.
            uid: unsafe { getuid() },
            birth_identity: String::new(),
        }
    }

    fn observed(row: crate::wake::ManagedProcessTopology) -> Self {
        Self {
            pid: row.pid,
            executable: row.executable_hint,
            uid: row.uid,
            birth_identity: row.birth_identity,
        }
    }
}

fn executable_in_gate_workset(executable: &str) -> bool {
    let name = Path::new(executable)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(executable);
    GATE_ORPHAN_WORKSET.iter().any(|entry| {
        entry.strip_suffix('*').map_or(name == *entry, |prefix| {
            name.starts_with(prefix) || (name.len() >= 12 && prefix.starts_with(name))
        })
    })
}

/// Keep the raw topology diff separate from the production reporting policy. The first vector is
/// eligible for `MechCheckFailed`; the second remains evidence-only noise.
pub fn partition_gate_orphans(orphans: &[GateOrphan]) -> (Vec<GateOrphan>, Vec<GateOrphan>) {
    // SAFETY: getuid has no preconditions and does not mutate process state.
    let current_uid = unsafe { getuid() };
    orphans.iter().cloned().partition(|orphan| {
        orphan.uid == current_uid && executable_in_gate_workset(&orphan.executable)
    })
}

/// Intersect two post-gate samples by durable process epoch, not by reusable PID alone.
pub fn persistent_gate_orphans(first: &[GateOrphan], second: &[GateOrphan]) -> Vec<GateOrphan> {
    second
        .iter()
        .filter(|candidate| {
            first.iter().any(|earlier| {
                earlier.pid == candidate.pid && earlier.birth_identity == candidate.birth_identity
            })
        })
        .cloned()
        .collect()
}

/// Append consumer-side observations next to the gate log without turning a snapshot failure into
/// a gate failure. Callers deliberately degrade to stderr if this evidence sink is unavailable.
pub fn append_gate_orphan_evidence(
    log_dir: &Path,
    tag: &str,
    name: &str,
    observation: &str,
) -> Result<PathBuf> {
    fs::create_dir_all(log_dir)?;
    let path = log_dir.join(format!("{tag}-gate-{name}.orphans"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open gate orphan evidence failed: {}", path.display()))?;
    let mut line = observation.as_bytes().to_vec();
    line.push(b'\n');
    file.write_all(&line)
        .with_context(|| format!("append gate orphan evidence failed: {}", path.display()))?;
    Ok(path)
}

/// Register one fixture group. Missing or empty environment input is a strict no-op.
pub fn register_gate_fixture(raw: Option<&OsStr>, pid: u32, pgid: u32) -> Result<Option<PathBuf>> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if pid == 0 || pgid == 0 {
        bail!("gate fixture pid and pgid must be non-zero");
    }
    let path = PathBuf::from(raw);
    let birth_identity = match crate::wake::managed_process_birth_identity(pgid) {
        Ok(Some(identity)) => identity,
        Ok(None) => UNKNOWN_BIRTH_IDENTITY.to_string(),
        Err(error) => {
            eprintln!(
                "gate fixture registry {} could not capture pgid {} birth identity: {error:#}; recording unknown",
                path.display(),
                pgid
            );
            UNKNOWN_BIRTH_IDENTITY.to_string()
        }
    };
    let birth_identity = birth_identity
        .replace('\t', "_")
        .replace('\r', "_")
        .replace('\n', "_");
    let line = format!("{pid}\t{pgid}\t{birth_identity}\n");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open gate fixture registry failed: {}", path.display()))?;
    // One O_APPEND write keeps concurrent fixture registrations as indivisible complete lines.
    file.write_all(line.as_bytes())
        .with_context(|| format!("append gate fixture registry failed: {}", path.display()))?;
    Ok(Some(path))
}

/// Reap every valid registered process group. Empty and torn lines are evidence, not hard errors.
pub fn reap_gate_fixture_registry(path: &Path) -> Result<Vec<u32>> {
    let mut control = SystemGateReapControl::new();
    reap_gate_fixture_registry_with_control(path, &mut control)
}

fn reap_gate_fixture_registry_with_control(
    path: &Path,
    control: &mut dyn GateReapControl,
) -> Result<Vec<u32>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read gate fixture registry failed: {}", path.display()))
        }
    };
    let mut processed = Vec::new();
    let mut skipped = 0usize;
    for (index, raw_line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if raw_line.last() != Some(&b'\n') {
            skipped += 1;
            eprintln!(
                "gate fixture registry {} skipped torn line {}",
                path.display(),
                index + 1
            );
            continue;
        }
        let line = String::from_utf8_lossy(&raw_line[..raw_line.len() - 1]);
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            skipped += 1;
            eprintln!(
                "gate fixture registry {} skipped empty line {}",
                path.display(),
                index + 1
            );
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        let parsed = (|| -> Option<(u32, u32, Option<&str>)> {
            if fields.len() != 3 {
                return None;
            }
            let pid = fields[0].parse().ok()?;
            let pgid = fields[1].parse().ok()?;
            if pid == 0 || pgid == 0 {
                return None;
            }
            let birth =
                (!fields[2].is_empty() && fields[2] != UNKNOWN_BIRTH_IDENTITY).then_some(fields[2]);
            Some((pid, pgid, birth))
        })();
        let Some((_pid, pgid, recorded_birth)) = parsed else {
            skipped += 1;
            eprintln!(
                "gate fixture registry {} skipped malformed line {}",
                path.display(),
                index + 1
            );
            continue;
        };
        processed.push(pgid);
        reap_gate_process_group_kernel(
            pgid,
            recorded_birth,
            ReapPolicy::RegisteredFixture,
            control,
        )?;
    }
    if skipped > 0 {
        eprintln!(
            "gate fixture registry {} skipped {} empty/torn/malformed line(s)",
            path.display(),
            skipped
        );
    }
    Ok(processed)
}

pub fn orphan_baseline() -> Result<BTreeSet<u32>> {
    Ok(crate::wake::managed_ppid1_pids()?.into_keys().collect())
}

pub fn orphans_since(baseline: &BTreeSet<u32>) -> Result<Vec<GateOrphan>> {
    Ok(crate::wake::managed_ppid1_pids()?
        .into_values()
        .filter(|row| !baseline.contains(&row.pid) && !row.zombie)
        .map(GateOrphan::observed)
        .collect())
}

pub fn orphan_failure_event(
    task_id: &str,
    round: &str,
    orphans: &[GateOrphan],
) -> orch_core::EventRecord {
    let reason = orphans
        .iter()
        .map(|orphan| format!("{}:{}", orphan.pid, orphan.executable))
        .collect::<Vec<_>>()
        .join(", ");
    crate::mech::failure_event(task_id, round, "gate-orphans", &reason)
}

/// Classify legacy process-group reap races: ESRCH(3) and EPERM(1) remain
/// tolerated at the syscall-classification seam.
#[doc(hidden)]
pub fn reap_errno_tolerated(errno: i32) -> bool {
    errno == 3 || errno == 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapPolicy {
    DirectGroup,
    RegisteredFixture,
}

#[doc(hidden)]
pub fn reap_errno_tolerated_for(policy: ReapPolicy, errno: i32) -> bool {
    match policy {
        ReapPolicy::DirectGroup => reap_errno_tolerated(errno),
        ReapPolicy::RegisteredFixture => errno == 3,
    }
}

pub const GATE_REAP_TOTAL_BUDGET: Duration = Duration::from_secs(2);
pub const GATE_REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateGroupMember {
    pid: u32,
    birth_identity: String,
    zombie: bool,
}

impl GateGroupMember {
    pub fn live(pid: u32, birth_identity: impl Into<String>) -> Self {
        Self {
            pid,
            birth_identity: birth_identity.into(),
            zombie: false,
        }
    }

    fn observed(pid: u32, birth_identity: String, zombie: bool) -> Self {
        Self {
            pid,
            birth_identity,
            zombie,
        }
    }

    fn diagnostic(&self) -> String {
        format!(
            "pid={} birth={} zombie={}",
            self.pid, self.birth_identity, self.zombie
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateGroupObservation {
    Gone,
    Members(Vec<GateGroupMember>),
    Ambiguous { reason: String },
}

pub trait GateReapControl {
    fn observe_group(&mut self, pgid: u32) -> std::result::Result<GateGroupObservation, String>;
    fn signal_kill(&mut self, pgid: u32) -> std::io::Result<()>;
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
}

struct SystemGateReapControl {
    started: Instant,
}

impl SystemGateReapControl {
    fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl GateReapControl for SystemGateReapControl {
    fn observe_group(&mut self, pgid: u32) -> std::result::Result<GateGroupObservation, String> {
        Ok(match crate::wake::observe_exact_gate_group_members(pgid) {
            Ok(None) => GateGroupObservation::Gone,
            Ok(Some(members)) => GateGroupObservation::Members(
                members
                    .into_iter()
                    .map(|(pid, birth, zombie)| GateGroupMember::observed(pid, birth, zombie))
                    .collect(),
            ),
            Err(error) => GateGroupObservation::Ambiguous {
                reason: format!("{error:#}"),
            },
        })
    }

    fn signal_kill(&mut self, pgid: u32) -> std::io::Result<()> {
        let pgid = i32::try_from(pgid).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process group id exceeds pid_t",
            )
        })?;
        // SAFETY: negative pid selects the exact process group.
        if unsafe { libc_kill(-pgid, 9) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn live_gate_group_members(
    pgid: u32,
    phase: &str,
    observation: GateGroupObservation,
) -> Result<Option<Vec<GateGroupMember>>> {
    match observation {
        GateGroupObservation::Gone => Ok(None),
        GateGroupObservation::Ambiguous { reason } => {
            bail!("gate process group {pgid} {phase} census ambiguous: {reason}")
        }
        GateGroupObservation::Members(members) => {
            if members.is_empty() {
                bail!("gate process group {pgid} {phase} census ambiguous: empty member set")
            }
            let mut pids = BTreeSet::new();
            for member in &members {
                if member.pid == 0 || member.birth_identity.is_empty() || !pids.insert(member.pid) {
                    bail!(
                        "gate process group {pgid} {phase} census ambiguous: invalid/duplicate member {}",
                        member.diagnostic()
                    );
                }
            }
            if members.iter().all(|member| member.zombie) {
                Ok(None)
            } else {
                Ok(Some(members))
            }
        }
    }
}

fn observe_live_gate_group(
    pgid: u32,
    phase: &str,
    control: &mut dyn GateReapControl,
) -> Result<Option<Vec<GateGroupMember>>> {
    let observation = control.observe_group(pgid).map_err(|reason| {
        anyhow::anyhow!("gate process group {pgid} {phase} census failed: {reason}")
    })?;
    live_gate_group_members(pgid, phase, observation)
}

fn validate_gate_group_epoch(
    pgid: u32,
    expected_leader_birth: Option<&str>,
    observed_epochs: &mut BTreeMap<u32, String>,
    members: &[GateGroupMember],
) -> Result<()> {
    for member in members {
        if let Some(previous_birth) = observed_epochs.get(&member.pid) {
            if previous_birth != &member.birth_identity {
                bail!(
                    "gate process group {pgid} member pid {} identity/birth drift: {} -> {}",
                    member.pid,
                    previous_birth,
                    member.birth_identity
                );
            }
        }
    }

    if let Some(leader) = members.iter().find(|member| member.pid == pgid) {
        match expected_leader_birth {
            Some(expected) if expected != leader.birth_identity.as_str() => {
                bail!(
                    "gate process group {pgid} leader identity/birth changed: expected={expected} observed={}",
                    leader.birth_identity
                )
            }
            None => {
                bail!(
                    "gate process group {pgid} acquired replacement leader birth {} after an initial leaderless direct census",
                    leader.birth_identity
                )
            }
            Some(_) => {}
        }
    }

    for member in members {
        observed_epochs
            .entry(member.pid)
            .or_insert_with(|| member.birth_identity.clone());
    }
    Ok(())
}

fn signal_gate_process_group(
    pgid: u32,
    attempts: &mut usize,
    policy: ReapPolicy,
    control: &mut dyn GateReapControl,
) -> Result<()> {
    *attempts = attempts.saturating_add(1);
    match control.signal_kill(pgid) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .raw_os_error()
                .is_some_and(|errno| reap_errno_tolerated_for(policy, errno)) =>
        {
            if error.raw_os_error() == Some(1) {
                eprintln!(
                    "gate process group {pgid} reap skipped: EPERM (not permitted for this gate)"
                );
            }
            Ok(())
        }
        Err(error) => {
            let source = match policy {
                ReapPolicy::DirectGroup => "direct",
                ReapPolicy::RegisteredFixture => "registry",
            };
            let errno = match error.raw_os_error() {
                Some(1) => "1 (EPERM)".to_string(),
                Some(errno) => errno.to_string(),
                None => "unknown".to_string(),
            };
            Err(error).with_context(|| {
                format!("reap gate process group failed: pgid={pgid} source={source} errno={errno}")
            })
        }
    }
}

fn gate_reap_timeout(pgid: u32, attempts: usize, survivors: &[GateGroupMember]) -> anyhow::Error {
    anyhow::anyhow!(
        "gate process group {pgid} failed to converge within {}ms: attempts={attempts}; survivors=[{}]",
        GATE_REAP_TOTAL_BUDGET.as_millis(),
        survivors
            .iter()
            .map(GateGroupMember::diagnostic)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
std::thread_local! {
    static B249_KERNEL_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn reap_gate_process_group_kernel(
    pgid: u32,
    anchor: Option<&str>,
    policy: ReapPolicy,
    control: &mut dyn GateReapControl,
) -> Result<()> {
    if pgid == 0 {
        bail!("process group id must be non-zero");
    }
    let _ = i32::try_from(pgid).context("process group id exceeds pid_t")?;
    #[cfg(test)]
    B249_KERNEL_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));

    let supplied_anchor_empty = anchor.is_some_and(str::is_empty);
    let anchor = anchor.filter(|birth| !birth.is_empty());
    let deadline = control.elapsed().saturating_add(GATE_REAP_TOTAL_BUDGET);
    let Some(initial_members) = observe_live_gate_group(pgid, "initial", control)? else {
        return Ok(());
    };
    if supplied_anchor_empty {
        bail!("gate process group {pgid} was given an empty birth anchor; refusing first signal");
    }
    if policy == ReapPolicy::RegisteredFixture && anchor.is_none() {
        bail!(
            "gate process group {pgid} registered fixture lacks a recorded birth anchor; refusing first signal"
        );
    }
    let initial_leader = initial_members.iter().find(|member| member.pid == pgid);
    if let (Some(expected), Some(observed)) = (anchor, initial_leader) {
        if expected != observed.birth_identity.as_str() {
            bail!(
                "gate process group {pgid} recorded anchor identity/birth mismatch: expected={expected} observed={}",
                observed.birth_identity
            );
        }
    }
    let expected_leader_birth = anchor
        .map(str::to_owned)
        .or_else(|| initial_leader.map(|member| member.birth_identity.clone()));
    let mut observed_epochs = BTreeMap::new();
    validate_gate_group_epoch(
        pgid,
        expected_leader_birth.as_deref(),
        &mut observed_epochs,
        &initial_members,
    )?;

    let mut attempts = 0usize;
    let mut survivors = initial_members;
    loop {
        if control.elapsed() >= deadline {
            return Err(gate_reap_timeout(pgid, attempts, &survivors));
        }
        signal_gate_process_group(pgid, &mut attempts, policy, control)?;
        let remaining = deadline.saturating_sub(control.elapsed());
        if !remaining.is_zero() {
            control.sleep(GATE_REAP_POLL_INTERVAL.min(remaining));
        }

        let Some(current_members) = observe_live_gate_group(pgid, "post-KILL", control)? else {
            return Ok(());
        };
        validate_gate_group_epoch(
            pgid,
            expected_leader_birth.as_deref(),
            &mut observed_epochs,
            &current_members,
        )?;
        survivors = current_members;
    }
}

/// Identity-safe convergence kernel shared by direct and registry cleanup.
#[doc(hidden)]
pub fn reap_gate_process_group_with_control(
    pgid: u32,
    anchor: Option<&str>,
    control: &mut dyn GateReapControl,
) -> Result<()> {
    reap_gate_process_group_kernel(pgid, anchor, ReapPolicy::DirectGroup, control)
}

/// Kill and prove convergence of an entire gate process group by pgrp id.
pub fn reap_gate_process_group(pgid: u32) -> Result<()> {
    let mut control = SystemGateReapControl::new();
    reap_gate_process_group_with_control(pgid, None, &mut control)
}

#[doc(hidden)]
pub mod b249_contract {
    pub use super::{
        reap_gate_process_group, reap_gate_process_group_with_control, GateGroupMember,
        GateGroupObservation, GateReapControl, GATE_REAP_POLL_INTERVAL, GATE_REAP_TOTAL_BUDGET,
    };
    pub use crate::gitx::{canonical_worktree_common_dir, worktree_add, worktree_add_detached};
}

pub struct GateResult {
    pub name: String,
    pub exit_code: i32,
    pub duration_ms: u128,
    pub log_path: String,
}

/// B58：门结果纯汇总（additive 纯函数，不改既有门逻辑）。
#[derive(Debug, PartialEq)]
pub struct GateSummary {
    pub total: usize,
    pub green: usize,
    pub red: usize,
    pub total_ms: u128,
}

/// 把一组门结果折叠成「绿/红/总数/总耗时」：exit_code == 0 计绿，!= 0（含 -1 哨兵）计红；
/// total_ms 为所有 duration_ms 之和；空切片全 0。
pub fn summarize_gate_results(results: &[GateResult]) -> GateSummary {
    let mut summary = GateSummary {
        total: results.len(),
        green: 0,
        red: 0,
        total_ms: 0,
    };
    for r in results {
        if r.exit_code == 0 {
            summary.green += 1;
        } else {
            summary.red += 1;
        }
        summary.total_ms += r.duration_ms;
    }
    summary
}

/// 合后门折叠摘要行（r32/B63）：用 summarize_gate_results 折叠成一行人读摘要。
/// 格式："合后门: {green} 绿 / {red} 红 / {total} 门 · {total_ms}ms"；空切片全 0。
pub fn render_gate_summary_line(results: &[GateResult]) -> String {
    let s = summarize_gate_results(results);
    format!(
        "合后门: {} 绿 / {} 红 / {} 门 · {}ms",
        s.green, s.red, s.total, s.total_ms
    )
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GateEvidence {
    pub key: String,
    pub exit_code: i32,
    pub log_sha256: String,
}

/// Store the raw gate log as a CAS object, then atomically update the key index to a JSON evidence
/// object. `cas::keyed_put` defines same-key semantics as latest-write-wins.
pub fn record_evidence(
    store: &cas::Store,
    key: &str,
    exit_code: i32,
    log: &[u8],
) -> Result<GateEvidence> {
    let evidence = GateEvidence {
        key: key.to_string(),
        exit_code,
        log_sha256: store.put(log)?,
    };
    let encoded = serde_json::to_vec(&evidence).context("序列化门证据失败")?;
    cas::keyed_put(store, key, &encoded)?;
    Ok(evidence)
}

/// Resolve keyed evidence. Missing, unreadable, malformed, or cross-key evidence is a cache miss;
/// the caller must run the gate rather than guessing that an old result is reusable.
pub fn lookup_evidence(store: &cas::Store, key: &str) -> Option<GateEvidence> {
    let encoded = cas::keyed_get(store, key).ok().flatten()?;
    let evidence: GateEvidence = serde_json::from_slice(&encoded).ok()?;
    (evidence.key == key).then_some(evidence)
}

/// Legitimate-reuse decision (design/06 §7): same key AND green cached evidence is a legal
/// reuse; a different key, no cache, or a cached red must execute. Reusing an old green under a
/// new key is strictly forbidden, and a red result never enjoys the cache.
#[derive(Debug, PartialEq)]
pub enum GatePlan {
    Execute,
    Reuse { exit_code: i32, log_sha256: String },
}

pub fn plan_gate_run(cached: Option<&GateEvidence>, current_key: &str) -> GatePlan {
    match cached {
        Some(ev) if ev.key == current_key && ev.exit_code == 0 => GatePlan::Reuse {
            exit_code: ev.exit_code,
            log_sha256: ev.log_sha256.clone(),
        },
        _ => GatePlan::Execute,
    }
}

/// Auditable disclosure line for a legitimate reuse. Silent reuse is a fabrication breeding
/// ground, so every skip must print this line.
fn reuse_disclosure(key: &str, log_sha256: &str) -> String {
    format!("cache-reuse key={key} log={log_sha256}")
}

/// Open one gate log and duplicate that handle for stdout/stderr.
///
/// `File::try_clone` preserves a shared underlying file offset on the supported Unix runtime, so
/// writes from either child stream advance the same cursor.  Opening the path twice would give
/// stdout a truncating cursor at byte zero and stderr an independent append cursor, allowing a
/// later stdout flood to overwrite early Cargo `Compiling ...` diagnostics.
pub fn gate_log_sinks(path: &Path) -> Result<(Stdio, Stdio)> {
    let stdout = File::options()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("创建门日志失败: {}", path.display()))?;
    let stderr = stdout
        .try_clone()
        .with_context(|| format!("复制门日志句柄失败: {}", path.display()))?;
    Ok((Stdio::from(stdout), Stdio::from(stderr)))
}

pub fn run_gate(
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    run_gate_command(
        name,
        spec,
        workdir,
        log_dir,
        tag,
        spec.timeout_seconds,
        None,
    )
}

/// Trial-only gate entry.  The marker is re-read before `log_dir` or the log file is created and
/// before the command is spawned.  Only this entry injects the private trial target, so main,
/// task-worktree and fixed-review gates can never accidentally receive a pooled target.
pub fn run_trial_gate(
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
    target: &buildcache::PreparedTrialTarget,
) -> Result<GateResult> {
    target.verify_before_spawn()?;
    run_gate_command(
        name,
        spec,
        workdir,
        log_dir,
        tag,
        buildcache::resolve_trial_timeout_secs(spec.timeout_seconds, spec.trial_timeout_seconds),
        Some(target.target_dir()),
    )
}

/// Fresh production gate boundary.  Its closed audit identity is acquired and
/// held with the storage permit until the child and its process group have
/// been reaped.
#[allow(clippy::too_many_arguments)]
pub fn run_gate_with_audit_identity(
    root: &Path,
    round: &str,
    identity: crate::ledger::GateAuditIdentity<'_>,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    let _storage_permit = crate::storage::guard_gate_operation(
        root,
        round,
        identity,
        &[
            workdir.to_path_buf(),
            log_dir.to_path_buf(),
            root.join("orch/target"),
        ],
    )?;
    run_gate(name, spec, workdir, log_dir, tag)
}

/// Production gate under a permit held by a wider workflow.  Callers must
/// freshly refresh that permit with the same closed identity immediately
/// before invoking this runner.
#[allow(clippy::too_many_arguments)]
pub fn run_gate_with_permit_and_identity(
    _permit: &crate::storage::StoragePermit,
    identity: crate::ledger::GateAuditIdentity<'_>,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    identity.validate()?;
    run_gate(name, spec, workdir, log_dir, tag)
}

/// Trial gate under a permit held by a wider workflow. Callers must freshly recheck that permit
/// with the same closed identity immediately before invoking this typed runner.
#[allow(clippy::too_many_arguments)]
pub fn run_trial_gate_with_permit_and_identity(
    _permit: &crate::storage::StoragePermit,
    identity: crate::ledger::GateAuditIdentity<'_>,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
    target: &buildcache::PreparedTrialTarget,
) -> Result<GateResult> {
    identity.validate()?;
    run_trial_gate(name, spec, workdir, log_dir, tag, target)
}

const DEFAULT_GATE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const ORCH_GATE_HEARTBEAT_INTERVAL_ENV: &str = "ORCH_GATE_HB_SECS";

pub fn gate_heartbeat_sidecar_path(log_dir: &Path, tag: &str, name: &str) -> PathBuf {
    log_dir.join(format!("{tag}-gate-{name}.hb"))
}

fn configured_gate_heartbeat_interval() -> Duration {
    std::env::var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_GATE_HEARTBEAT_INTERVAL)
}

#[derive(Default)]
struct GateLogProgress {
    offset: u64,
    complete_lines: u64,
    has_partial_line: bool,
}

fn observe_gate_log_lastline(sidecar: &Path, progress: &mut GateLogProgress) -> Option<u64> {
    let mut log = File::open(sidecar.with_extension("log")).ok()?;
    let length = log.metadata().ok()?.len();
    if length < progress.offset {
        *progress = GateLogProgress::default();
    }
    log.seek(SeekFrom::Start(progress.offset)).ok()?;
    let mut buffer = [0u8; 8192];
    loop {
        let read = log.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        progress.offset = progress.offset.saturating_add(read as u64);
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                progress.complete_lines = progress.complete_lines.saturating_add(1);
                progress.has_partial_line = false;
            } else {
                progress.has_partial_line = true;
            }
        }
    }
    Some(
        progress
            .complete_lines
            .saturating_add(u64::from(progress.has_partial_line)),
    )
}

pub struct GateHeartbeatHandle {
    stop_sender: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl GateHeartbeatHandle {
    fn idle() -> Self {
        Self {
            stop_sender: None,
            thread: None,
        }
    }

    fn finish(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("gate heartbeat thread ended unexpectedly");
            }
        }
    }

    pub fn stop(mut self) {
        self.finish();
    }
}

impl Drop for GateHeartbeatHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn spawn_gate_heartbeat(
    sidecar: &Path,
    child_pid: u32,
    interval: Duration,
) -> GateHeartbeatHandle {
    let interval = if interval.is_zero() {
        DEFAULT_GATE_HEARTBEAT_INTERVAL
    } else {
        interval
    };
    let mut sidecar_file = match OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(sidecar)
    {
        Ok(file) => file,
        Err(failure) => {
            eprintln!(
                "gate heartbeat sidecar unavailable at {}: {failure}",
                sidecar.display()
            );
            return GateHeartbeatHandle::idle();
        }
    };
    let sidecar_path = sidecar.to_path_buf();
    let sidecar_display = sidecar.display().to_string();
    let (stop_sender, stop_receiver) = mpsc::channel();
    let builder = std::thread::Builder::new().name(format!("gate-hb-{child_pid}"));
    let thread = match std::thread::Builder::spawn(builder, move || {
        let started = Instant::now();
        let mut progress = GateLogProgress::default();
        loop {
            match stop_receiver.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let lastline = observe_gate_log_lastline(&sidecar_path, &mut progress)
                        .map_or_else(|| "?".to_string(), |line| line.to_string());
                    let line = format!(
                        "[gate-hb] t=+{}s child={child_pid} state=? lastline={lastline}\n",
                        started.elapsed().as_secs()
                    );
                    if let Err(failure) = sidecar_file.write_all(line.as_bytes()) {
                        eprintln!(
                            "gate heartbeat sidecar write stopped at {sidecar_display}: {failure}"
                        );
                        break;
                    }
                }
            }
        }
    }) {
        Ok(thread) => thread,
        Err(failure) => {
            eprintln!("gate heartbeat thread unavailable: {failure}");
            return GateHeartbeatHandle::idle();
        }
    };
    GateHeartbeatHandle {
        stop_sender: Some(stop_sender),
        thread: Some(thread),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_gate_command(
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
    timeout_seconds: u64,
    cargo_target_dir: Option<&Path>,
) -> Result<GateResult> {
    fs::create_dir_all(log_dir)?;
    let log_path = log_dir.join(format!("{tag}-gate-{name}.log"));
    let registry_path = log_dir.join(format!("{tag}-gate-{name}.fixtures"));
    let (prog, rest) = spec
        .argv
        .split_first()
        .with_context(|| format!("门 {name} argv 为空"))?;
    let mut cmd = Command::new(prog);
    cmd.args(rest).current_dir(workdir).stdin(Stdio::null());
    if let Some(target) = cargo_target_dir {
        cmd.env("CARGO_TARGET_DIR", target);
    }
    cmd.env(ORCH_GATE_FIXTURE_REGISTRY, &registry_path);
    #[cfg(unix)]
    cmd.process_group(0);
    let (stdout, stderr) = gate_log_sinks(&log_path)?;
    cmd.stdout(stdout).stderr(stderr);
    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("门 {name} spawn 失败: {prog}"))?;
    let pgid = child.id();
    let heartbeat_path = gate_heartbeat_sidecar_path(log_dir, tag, name);
    let heartbeat =
        spawn_gate_heartbeat(&heartbeat_path, pgid, configured_gate_heartbeat_interval());
    let status = match child.wait_timeout(Duration::from_secs(timeout_seconds))? {
        Some(s) => {
            reap_gate_process_group(pgid)?;
            reap_gate_fixture_registry(&registry_path)?;
            heartbeat.stop();
            s
        }
        None => {
            reap_gate_process_group(pgid)?;
            reap_gate_fixture_registry(&registry_path)?;
            child.wait().ok();
            heartbeat.stop();
            bail!("门 {name} 超时（>{timeout_seconds}s）");
        }
    };
    Ok(GateResult {
        name: name.to_string(),
        exit_code: status.code().unwrap_or(-1),
        duration_ms: start.elapsed().as_millis(),
        log_path: log_path.display().to_string(),
    })
}

/// Production gate wrapper. `run_gate` remains a pure command runner for isolated tests; runtime
/// entries with root/round context acquire admission before log creation or child spawn.
#[allow(clippy::too_many_arguments)]
pub fn run_gate_guarded(
    root: &Path,
    round: &str,
    task: Option<&str>,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    let _storage_permit = crate::storage::guard_operation(
        root,
        round,
        task,
        None,
        crate::storage::GuardEntry::Gate,
        &[
            workdir.to_path_buf(),
            log_dir.to_path_buf(),
            root.join("orch/target"),
        ],
    )?;
    run_gate(name, spec, workdir, log_dir, tag)
}

/// Execute a gate under a permit acquired by a wider atomic workflow (collect also covers seed
/// replay and trial-worktree creation). Requiring the token in the type signature prevents a
/// caller from accidentally moving the spawn outside that admission scope.
pub fn run_gate_with_permit(
    _permit: &crate::storage::StoragePermit,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    run_gate(name, spec, workdir, log_dir, tag)
}

/// Gate execution with evidence recording and legitimate-reuse short-circuit: `plan_gate_run`
/// decides before anything runs. A legal `Reuse` skips the real command, adopts the cached exit
/// code as the gate outcome, and prints a `cache-reuse` disclosure line so the skip stays
/// auditable. `Execute` runs the gate and records fresh evidence exactly as before; a non-reusable
/// hit (same key but red) is still disclosed.
pub fn run_gate_with_evidence(
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
    store: &cas::Store,
    key: &str,
) -> Result<(GateResult, GateEvidence)> {
    let cached = lookup_evidence(store, key);
    if let GatePlan::Reuse {
        exit_code,
        ref log_sha256,
    } = plan_gate_run(cached.as_ref(), key)
    {
        println!("{}", reuse_disclosure(key, log_sha256));
        let result = GateResult {
            name: name.to_string(),
            exit_code,
            duration_ms: 0,
            log_path: format!("cache-reuse:{log_sha256}"),
        };
        return Ok((result, cached.expect("Reuse 蕴含缓存命中")));
    }
    if let Some(hit) = &cached {
        println!(
            "gate cache-hit · key={} · exit={} · log={}（仅披露，仍执行）",
            hit.key, hit.exit_code, hit.log_sha256
        );
    }
    let result = run_gate(name, spec, workdir, log_dir, tag)?;
    let log = fs::read(&result.log_path)
        .with_context(|| format!("读取门日志失败: {}", result.log_path))?;
    let evidence = record_evidence(store, key, result.exit_code, &log)?;
    Ok((result, evidence))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct ProcessGroupCleanup(u32);

    #[cfg(unix)]
    impl Drop for ProcessGroupCleanup {
        fn drop(&mut self) {
            let reap = reap_gate_process_group;
            let _ = reap(self.0);
        }
    }

    #[cfg(unix)]
    fn process_group_exists(pgid: u32) -> bool {
        let pgid = i32::try_from(pgid).expect("test pgid fits pid_t");
        // SAFETY: signal 0 does not alter the target process group; it only probes existence.
        if unsafe { libc_kill(-pgid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(3)
    }

    fn store(tag: &str) -> (std::path::PathBuf, cas::Store) {
        let dir = crate::util::test_scratch_dir(&format!("gate-evidence-{tag}"));
        (dir.clone(), cas::Store::new(&dir))
    }

    struct InjectedKillControl {
        observations: std::collections::VecDeque<GateGroupObservation>,
        signal_errno: i32,
        elapsed: Duration,
        signaled_pgids: Vec<u32>,
    }

    impl InjectedKillControl {
        fn eperm(observations: impl IntoIterator<Item = GateGroupObservation>) -> Self {
            Self {
                observations: observations.into_iter().collect(),
                signal_errno: 1,
                elapsed: Duration::ZERO,
                signaled_pgids: Vec::new(),
            }
        }
    }

    impl GateReapControl for InjectedKillControl {
        fn observe_group(
            &mut self,
            pgid: u32,
        ) -> std::result::Result<GateGroupObservation, String> {
            self.observations
                .pop_front()
                .ok_or_else(|| format!("unexpected extra census for pgid {pgid}"))
        }

        fn signal_kill(&mut self, pgid: u32) -> std::io::Result<()> {
            self.signaled_pgids.push(pgid);
            Err(std::io::Error::from_raw_os_error(self.signal_errno))
        }

        fn elapsed(&self) -> Duration {
            self.elapsed
        }

        fn sleep(&mut self, duration: Duration) {
            self.elapsed = self.elapsed.saturating_add(duration);
        }
    }

    #[test]
    fn b248_direct_production_path_tolerates_injected_eperm() {
        let pgid = 42_248;
        let mut control = InjectedKillControl::eperm([
            GateGroupObservation::Members(vec![GateGroupMember::live(pgid, "direct-birth")]),
            GateGroupObservation::Gone,
        ]);

        reap_gate_process_group_with_control(pgid, None, &mut control)
            .expect("direct production wrapper must tolerate EPERM from the kill syscall seam");
        assert_eq!(control.signaled_pgids, vec![pgid]);
    }

    #[test]
    fn b248_registry_production_path_hard_fails_on_injected_eperm() {
        let dir = crate::util::test_scratch_dir("b248-registry-eperm");
        let registry = dir.join("fixtures");
        let pgid = 42_249;
        fs::write(&registry, format!("{pgid}\t{pgid}\tregistry-birth\n")).unwrap();
        let mut control = InjectedKillControl::eperm([
            GateGroupObservation::Members(vec![GateGroupMember::live(pgid, "registry-birth")]),
            GateGroupObservation::Gone,
        ]);

        let result = reap_gate_fixture_registry_with_control(&registry, &mut control);
        fs::remove_dir_all(dir).unwrap();
        let error = result
            .expect_err("registry production body must hard-fail on EPERM from the kill seam");
        let trace = format!("{error:#}");
        assert!(trace.contains(&format!("pgid={pgid}")), "trace={trace}");
        assert!(trace.contains("source=registry"), "trace={trace}");
        assert!(trace.contains("errno=1 (EPERM)"), "trace={trace}");
        assert_eq!(control.signaled_pgids, vec![pgid]);
    }

    #[test]
    fn b249_direct_and_registry_production_paths_share_one_kernel() {
        let dir = crate::util::test_scratch_dir("b249-production-kernel-wiring");
        let registry = dir.join("fixtures");
        let mut child = Command::new("/usr/bin/true")
            .process_group(0)
            .spawn()
            .expect("spawn short-lived process group");
        let pgid = child.id();
        child.wait().expect("wait short-lived process group");

        let before = B249_KERNEL_CALLS.with(std::cell::Cell::get);
        reap_gate_process_group(pgid).expect("direct path should observe an already-gone group");
        fs::write(
            &registry,
            format!("{pgid}\t{pgid}\t{UNKNOWN_BIRTH_IDENTITY}\n"),
        )
        .unwrap();
        assert_eq!(reap_gate_fixture_registry(&registry).unwrap(), vec![pgid]);
        let after = B249_KERNEL_CALLS.with(std::cell::Cell::get);
        assert_eq!(
            after - before,
            2,
            "direct and registry production wrappers must each enter the shared convergence kernel"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn production_gate_log_preserves_early_stderr_during_stdout_flood() {
        let dir = crate::util::test_scratch_dir("b232-production-gate-log");
        let spec = CommandSpec {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'Compiling orch-core v0.0.0 (B232-GREEN)' 1>&2; dd if=/dev/zero bs=1024 count=128 2>/dev/null | tr '\\0' x"
                    .into(),
            ],
            timeout_seconds: 5,
            trial_timeout_seconds: None,
            approval: None,
        };
        let result = run_gate("b232-log", &spec, &dir, &dir.join("logs"), "trial").unwrap();
        let log = fs::read_to_string(&result.log_path).unwrap();
        let heartbeat = fs::read(gate_heartbeat_sidecar_path(
            &dir.join("logs"),
            "trial",
            "b232-log",
        ))
        .unwrap();
        println!(
            "B232_PRODUCTION_LOG_PATH={} bytes={} compiling_present={} heartbeat_bytes={}",
            result.log_path,
            log.len(),
            log.contains("Compiling orch-core v0.0.0 (B232-GREEN)"),
            heartbeat.len()
        );
        assert!(
            log.contains("Compiling orch-core v0.0.0 (B232-GREEN)"),
            "production gate log must retain early stderr diagnostics"
        );
        assert!(log.len() >= 128 * 1024, "stdout flood must remain complete");
        assert!(
            !log.lines().any(|line| line.starts_with("[gate-hb]")),
            "heartbeat bytes must never enter the gate log"
        );
        assert!(
            heartbeat.is_empty(),
            "a gate shorter than the first interval must leave an empty sidecar"
        );
    }

    fn assert_heartbeat_sidecar(text: &str) {
        let lines = text
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert!(
            !lines.is_empty(),
            "heartbeat evidence must contain at least one line"
        );
        for line in lines {
            assert!(
                line.starts_with("[gate-hb] t=+"),
                "unexpected heartbeat: {line}"
            );
            assert!(line.len() <= 160, "heartbeat exceeds 160 bytes: {line}");
            assert!(
                !line.contains("error"),
                "heartbeat contains a reserved word: {line}"
            );
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_err(),
                "heartbeat must not be JSON: {line}"
            );
        }
    }

    #[test]
    #[ignore = "B244 evidence probe: runs the binding's complete testFast gate"]
    fn b244_real_testfast_sidecar_probe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf();
        let binding = crate::binding::load(&root).unwrap();
        let spec = binding.commands.get("testFast").unwrap();
        let logs = crate::util::test_scratch_dir("b244-real-testfast");
        let previous = std::env::var_os(ORCH_GATE_HEARTBEAT_INTERVAL_ENV);
        std::env::set_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV, "1");
        let result = run_gate("testFast", spec, &root, &logs, "B244-probe");
        match previous {
            Some(value) => std::env::set_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV, value),
            None => std::env::remove_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV),
        }
        let result = result.unwrap();
        let heartbeat_path = gate_heartbeat_sidecar_path(&logs, "B244-probe", "testFast");
        let heartbeat = fs::read_to_string(&heartbeat_path).unwrap();
        let gate_log = fs::read_to_string(&result.log_path).unwrap();
        assert_eq!(result.exit_code, 0);
        assert_heartbeat_sidecar(&heartbeat);
        assert!(!gate_log.lines().any(|line| line.starts_with("[gate-hb]")));
        println!(
            "B244_REAL_TESTFAST duration_ms={} log_bytes={} heartbeat_bytes={} heartbeat_lines={} log={} sidecar={}",
            result.duration_ms,
            gate_log.len(),
            heartbeat.len(),
            heartbeat.lines().count(),
            result.log_path,
            heartbeat_path.display()
        );
    }

    #[test]
    #[ignore = "B244 evidence probe: measures a five-second fixture twice"]
    fn b244_heartbeat_io_tax_probe() {
        let dir = crate::util::test_scratch_dir("b244-heartbeat-io-tax");
        let logs = dir.join("logs");
        let spec = CommandSpec {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "sleep 5; printf 'stable gate output\\n'".into(),
            ],
            timeout_seconds: 10,
            trial_timeout_seconds: None,
            approval: None,
        };
        let previous = std::env::var_os(ORCH_GATE_HEARTBEAT_INTERVAL_ENV);
        std::env::set_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV, "3600");
        let control = run_gate("io-tax", &spec, &dir, &logs, "control");
        std::env::set_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV, "1");
        let active = run_gate("io-tax", &spec, &dir, &logs, "active");
        match previous {
            Some(value) => std::env::set_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV, value),
            None => std::env::remove_var(ORCH_GATE_HEARTBEAT_INTERVAL_ENV),
        }
        let control = control.unwrap();
        let active = active.unwrap();
        assert_eq!(control.exit_code, 0);
        assert_eq!(active.exit_code, 0);
        let control_log = fs::read(&control.log_path).unwrap();
        let active_log = fs::read(&active.log_path).unwrap();
        let control_heartbeat =
            fs::read(gate_heartbeat_sidecar_path(&logs, "control", "io-tax")).unwrap();
        let active_heartbeat =
            fs::read_to_string(gate_heartbeat_sidecar_path(&logs, "active", "io-tax")).unwrap();
        assert_eq!(control_log, active_log);
        assert!(control_heartbeat.is_empty());
        assert_heartbeat_sidecar(&active_heartbeat);
        let delta_ms = active.duration_ms as i128 - control.duration_ms as i128;
        println!(
            "B244_IO_TAX control_ms={} active_ms={} delta_ms={} heartbeat_bytes={} heartbeat_lines={}",
            control.duration_ms,
            active.duration_ms,
            delta_ms,
            active_heartbeat.len(),
            active_heartbeat.lines().count()
        );
    }

    #[cfg(unix)]
    #[test]
    fn gate_reaps_background_grandchild_after_leader_exits() {
        let dir = crate::util::test_scratch_dir("gate-background-grandchild");
        let pid_file = dir.join("pids");
        let spec = CommandSpec {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "/bin/sleep 30 & child=$!; printf '%s %s' \"$$\" \"$child\" > \"$1\"; exit 0"
                    .into(),
                "gate-background-grandchild".into(),
                pid_file.display().to_string(),
            ],
            timeout_seconds: 5,
            trial_timeout_seconds: None,
            approval: None,
        };

        let result = run_gate(
            "background-grandchild",
            &spec,
            &dir,
            &dir.join("logs"),
            "selftest",
        )
        .unwrap();
        assert_eq!(result.exit_code, 0, "gate leader should exit naturally");

        let pids =
            fs::read_to_string(&pid_file).expect("gate command records leader and child pids");
        let mut pids = pids.split_whitespace();
        let pgid = pids
            .next()
            .expect("leader pid")
            .parse::<u32>()
            .expect("numeric leader pid");
        let grandchild = pids
            .next()
            .expect("grandchild pid")
            .parse::<u32>()
            .expect("numeric grandchild pid");
        assert_ne!(pgid, grandchild, "fixture must create a real grandchild");
        let _cleanup = ProcessGroupCleanup(pgid);

        for _ in 0..100 {
            if !process_group_exists(pgid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("gate process group {pgid} still exists after reaping grandchild {grandchild}");
    }

    #[test]
    fn evidence_json_roundtrip_preserves_log_and_fields() {
        let (_dir, store) = store("roundtrip");
        let evidence = record_evidence(&store, "key-a", 7, b"gate-log").unwrap();

        assert_eq!(
            evidence,
            GateEvidence {
                key: "key-a".into(),
                exit_code: 7,
                log_sha256: evidence.log_sha256.clone(),
            }
        );
        assert_eq!(
            store.get(&evidence.log_sha256).unwrap(),
            Some(b"gate-log".to_vec())
        );
        assert_eq!(lookup_evidence(&store, "key-a"), Some(evidence));
    }

    #[test]
    fn empty_log_is_a_real_content_addressed_artifact() {
        let (_dir, store) = store("empty");
        let evidence = record_evidence(&store, "empty-key", 0, b"").unwrap();

        assert_eq!(store.get(&evidence.log_sha256).unwrap(), Some(Vec::new()));
        assert_eq!(lookup_evidence(&store, "empty-key"), Some(evidence));
    }

    #[test]
    fn malformed_or_cross_key_evidence_fails_closed_to_miss() {
        let (_dir, store) = store("invalid");
        cas::keyed_put(&store, "bad-json", b"not-json").unwrap();
        assert_eq!(lookup_evidence(&store, "bad-json"), None);

        let wrong = serde_json::to_vec(&GateEvidence {
            key: "other-key".into(),
            exit_code: 0,
            log_sha256: "deadbeef".into(),
        })
        .unwrap();
        cas::keyed_put(&store, "requested-key", &wrong).unwrap();
        assert_eq!(lookup_evidence(&store, "requested-key"), None);
    }

    #[test]
    fn cache_hit_is_disclosed_but_does_not_skip_gate_execution() {
        let (dir, store) = store("execute");
        record_evidence(&store, "same-key", 1, b"old-log").unwrap();
        let spec = CommandSpec {
            argv: vec!["/usr/bin/printf".into(), "fresh-log".into()],
            timeout_seconds: 5,
            trial_timeout_seconds: None,
            approval: None,
        };

        let (result, evidence) = run_gate_with_evidence(
            "probe",
            &spec,
            &dir,
            &dir.join("logs"),
            "selftest",
            &store,
            "same-key",
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(evidence.exit_code, 0);
        assert_eq!(
            store.get(&evidence.log_sha256).unwrap(),
            Some(b"fresh-log".to_vec())
        );
        assert_eq!(lookup_evidence(&store, "same-key"), Some(evidence));
    }

    #[test]
    fn reuse_disclosure_line_contains_key_and_log() {
        let line = reuse_disclosure("head-abc", "deadbeef");
        assert!(line.starts_with("cache-reuse "), "披露行须可机检: {line}");
        assert!(line.contains("key=head-abc"), "披露行须含键: {line}");
        assert!(line.contains("log=deadbeef"), "披露行须含日志指纹: {line}");
    }

    #[test]
    fn green_cache_reuse_skips_execution_and_returns_cached_outcome() {
        let (dir, store) = store("reuse");
        let cached = record_evidence(&store, "green-key", 0, b"cached-green-log").unwrap();
        // /usr/bin/false 一旦真跑必然 exit 1；复用路径下它绝不能被执行。
        let spec = CommandSpec {
            argv: vec!["/usr/bin/false".into()],
            timeout_seconds: 5,
            trial_timeout_seconds: None,
            approval: None,
        };
        let log_dir = dir.join("logs-reuse");

        let (result, evidence) = run_gate_with_evidence(
            "probe",
            &spec,
            &dir,
            &log_dir,
            "selftest",
            &store,
            "green-key",
        )
        .unwrap();

        assert_eq!(result.exit_code, 0, "复用须采用缓存 exit_code");
        assert_eq!(result.duration_ms, 0, "未真跑不得有耗时");
        assert!(
            result.log_path.starts_with("cache-reuse:"),
            "复用结果须自指缓存证据而非伪日志: {}",
            result.log_path
        );
        assert_eq!(evidence, cached, "复用不得改写证据");
        assert!(!log_dir.exists(), "真跑才会建日志目录，复用路径不得建");
        assert_eq!(lookup_evidence(&store, "green-key"), Some(cached));
    }

    #[test]
    fn consecutive_reuse_is_idempotent() {
        let (dir, store) = store("reuse-twice");
        let cached = record_evidence(&store, "green-key", 0, b"cached-green-log").unwrap();
        let spec = CommandSpec {
            argv: vec!["/usr/bin/false".into()],
            timeout_seconds: 5,
            trial_timeout_seconds: None,
            approval: None,
        };
        let log_dir = dir.join("logs-reuse-twice");

        let first = run_gate_with_evidence(
            "probe",
            &spec,
            &dir,
            &log_dir,
            "selftest",
            &store,
            "green-key",
        )
        .unwrap();
        let second = run_gate_with_evidence(
            "probe",
            &spec,
            &dir,
            &log_dir,
            "selftest",
            &store,
            "green-key",
        )
        .unwrap();

        assert_eq!(first.1, second.1, "连续两次复用证据须幂等");
        assert_eq!(first.0.exit_code, second.0.exit_code);
        assert_eq!(first.0.log_path, second.0.log_path);
        assert_eq!(second.1, cached);
        assert_eq!(lookup_evidence(&store, "green-key"), Some(cached));
        assert!(!log_dir.exists(), "两次复用均不得真跑");
    }

    #[test]
    fn trial_marker_mismatch_refuses_before_log_creation_or_spawn() {
        let dir = crate::util::test_scratch_dir("gate-trial-marker-preflight");
        let log_dir = dir.join("logs");
        let spawned = dir.join("spawned");
        let identity = buildcache::BuildIdentity {
            schema: 1,
            kind: buildcache::SlotKind::TrialStaging,
            canonical_source_root: "/stable/trial/source".into(),
            canonical_common_dir: "/repo/.git".into(),
            rustc_version: "rustc 1.99.0".into(),
            cargo_version: "cargo 1.99.0".into(),
            target_triple: "aarch64-apple-darwin".into(),
            cargo_lock_sha256: "lock-a".into(),
            build_config_digest: "cfg-a".into(),
        };
        buildcache::with_trial_slot(&dir, "B208", |slot| {
            let target = buildcache::prepare_trial_target(slot, &identity, None)?;
            fs::write(target.target_dir().join(buildcache::MARKER_FILE), b"{}")?;
            let spec = CommandSpec {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("touch '{}'", spawned.display()),
                ],
                timeout_seconds: 1,
                trial_timeout_seconds: Some(3),
                approval: None,
            };
            let error = match run_trial_gate(
                "probe",
                &spec,
                &dir,
                &log_dir,
                "identity-mismatch",
                &target,
            ) {
                Ok(_) => panic!("mismatched marker unexpectedly spawned the trial gate"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("refused before log/spawn"), "{error}");
            assert!(
                !log_dir.exists(),
                "identity refusal must precede log directory creation"
            );
            assert!(
                !spawned.exists(),
                "identity refusal must precede command spawn"
            );
            Ok(())
        })
        .unwrap();
    }

    fn wait_for_group_gone(pgid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !process_group_exists(pgid) {
                return true;
            }
            // After SIGKILL, an adopted member can remain as a zombie briefly, and high-churn
            // test runs can reuse the numeric PGID. Neither means the original group still has
            // a live escapee. Once its leader has been waited, every surviving original member
            // is init-adopted, so the topology projection distinguishes both cases safely.
            if crate::wake::managed_ppid1_pids()
                .is_ok_and(|rows| !rows.values().any(|row| row.pgid == pgid && !row.zombie))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        !process_group_exists(pgid)
            || crate::wake::managed_ppid1_pids()
                .is_ok_and(|rows| !rows.values().any(|row| row.pgid == pgid && !row.zombie))
    }

    fn registered_pgids(path: &Path) -> Vec<u32> {
        fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read registry {}: {error}", path.display()))
            .lines()
            .filter_map(|line| line.split('\t').nth(1)?.parse().ok())
            .collect()
    }

    #[test]
    fn b239_registered_fixture_runtime_child() {
        let Some(mode) = std::env::var_os("ORCH_B239_RUNTIME_CHILD_MODE") else {
            return;
        };
        let fixture = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do sleep 60; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn independently grouped runtime fixture");
        let pgid = fixture.id();
        register_gate_fixture(
            std::env::var_os(ORCH_GATE_FIXTURE_REGISTRY).as_deref(),
            pgid,
            pgid,
        )
        .expect("register independently grouped runtime fixture");
        // Child handles do not terminate their process on drop. Deliberately leave this fixture
        // alive so the enclosing gate's normal/timeout exit must consume the registry.
        std::mem::forget(fixture);
        if mode == "timeout" {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    fn runtime_fixture_spec(mode: &str, timeout_seconds: u64) -> CommandSpec {
        CommandSpec {
            argv: vec![
                "/usr/bin/env".into(),
                format!("ORCH_B239_RUNTIME_CHILD_MODE={mode}"),
                std::env::current_exe()
                    .expect("resolve current libtest executable")
                    .to_string_lossy()
                    .into_owned(),
                "--exact".into(),
                "gate::tests::b239_registered_fixture_runtime_child".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            timeout_seconds,
            trial_timeout_seconds: None,
            approval: None,
        }
    }

    #[test]
    #[ignore = "testExclusive:gate::tests::registry_reap_executes_on_the_normal_gate_exit_at_runtime"]
    fn registry_reap_executes_on_the_normal_gate_exit_at_runtime() {
        let dir = crate::util::test_scratch_dir("b239-runtime-normal-exit");
        let logs = dir.join("logs");
        let result = run_gate(
            "normal",
            &runtime_fixture_spec("normal", 10),
            &dir,
            &logs,
            "b239",
        )
        .expect("normal runtime gate should finish");
        assert_eq!(result.exit_code, 0);
        let registry = logs.join("b239-gate-normal.fixtures");
        let pgids = registered_pgids(&registry);
        assert_eq!(
            pgids.len(),
            1,
            "normal exit must consume a real registry line"
        );
        let _cleanup = ProcessGroupCleanup(pgids[0]);
        assert!(
            wait_for_group_gone(pgids[0]),
            "normal gate exit left registered process group {} alive",
            pgids[0]
        );
        println!(
            "B239_RUNTIME_EXIT kind=normal registry={} pgid={} alive_after=false",
            registry.display(),
            pgids[0]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "testExclusive:gate::tests::registry_reap_executes_on_the_timeout_gate_exit_at_runtime"]
    fn registry_reap_executes_on_the_timeout_gate_exit_at_runtime() {
        let dir = crate::util::test_scratch_dir("b239-runtime-timeout-exit");
        let logs = dir.join("logs");
        let error = match run_gate(
            "timeout",
            &runtime_fixture_spec("timeout", 1),
            &dir,
            &logs,
            "b239",
        ) {
            Ok(_) => panic!("timeout runtime gate unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("超时"), "{error:#}");
        let registry = logs.join("b239-gate-timeout.fixtures");
        let pgids = registered_pgids(&registry);
        assert_eq!(
            pgids.len(),
            1,
            "timeout exit must consume a real registry line"
        );
        let _cleanup = ProcessGroupCleanup(pgids[0]);
        assert!(
            wait_for_group_gone(pgids[0]),
            "timeout gate exit left registered process group {} alive",
            pgids[0]
        );
        println!(
            "B239_RUNTIME_EXIT kind=timeout registry={} pgid={} alive_after=false",
            registry.display(),
            pgids[0]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cross_group_fixture_escapes_gate_group_but_not_registry_reap() {
        let dir = crate::util::test_scratch_dir("b239-cross-group-runtime");
        let registry = dir.join("cross-group.fixtures");
        let mut fixture = Command::new("/bin/sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let fixture_pgid = fixture.id();
        let _cleanup = ProcessGroupCleanup(fixture_pgid);
        let mut fixture_member = Command::new("/bin/sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(fixture_pgid as i32)
            .spawn()
            .expect("spawn a fixed second member in the fixture group");
        let mut gate_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let gate_pgid = gate_child.id();
        gate_child.wait().unwrap();
        reap_gate_process_group(gate_pgid).unwrap();
        assert!(
            process_group_exists(fixture_pgid),
            "a different process group must escape gate-group-only cleanup"
        );
        register_gate_fixture(Some(registry.as_os_str()), fixture_pgid, fixture_pgid).unwrap();
        reap_gate_fixture_registry(&registry).unwrap();
        let terminated = fixture
            .wait_timeout(Duration::from_secs(10))
            .unwrap()
            .is_some();
        let member_terminated = fixture_member
            .wait_timeout(Duration::from_secs(10))
            .unwrap()
            .is_some();
        assert!(
            terminated,
            "registry cleanup must terminate the escaped group leader"
        );
        assert!(
            member_terminated,
            "registry cleanup must terminate every member of the escaped process group"
        );
        println!(
            "B239_CROSS_GROUP pgid={} members=2 alive_after_gate_reap=true alive_after_registry_reap=false",
            fixture_pgid
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn changed_birth_identity_prevents_a_stale_registry_kill() {
        let dir = crate::util::test_scratch_dir("b239-stale-registry");
        let registry = dir.join("stale.fixtures");
        let mut fixture = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do sleep 60; done"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = fixture.id();
        let _cleanup = ProcessGroupCleanup(pgid);
        assert!(
            crate::wake::managed_process_birth_identity(pgid)
                .unwrap()
                .is_some(),
            "supported runtime must expose a fixture birth identity"
        );
        fs::write(&registry, format!("{pgid}\t{pgid}\tdeliberately-stale\n")).unwrap();
        let error = reap_gate_fixture_registry(&registry)
            .expect_err("a stale registry anchor must fail before signalling");
        assert!(
            error.to_string().contains("anchor") || error.to_string().contains("identity"),
            "unexpected stale-anchor diagnostic: {error:#}"
        );
        assert!(
            process_group_exists(pgid),
            "positive birth-identity mismatch must refuse the signal"
        );
        reap_gate_process_group(pgid).unwrap();
        let _ = fixture.wait();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn torn_and_malformed_registry_lines_are_evidence_not_errors() {
        let dir = crate::util::test_scratch_dir("b239-torn-registry");
        let registry = dir.join("torn.fixtures");
        fs::write(&registry, b"\nnot-a-registry-line\n4242\t").unwrap();
        assert!(
            reap_gate_fixture_registry(&registry).unwrap().is_empty(),
            "no malformed line may become a signal target"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn consumer_filter_reports_only_current_uid_gate_workset_rows() {
        // SAFETY: getuid has no preconditions and does not mutate process state.
        let uid = unsafe { getuid() };
        let rows = vec![
            GateOrphan {
                pid: 1,
                executable: "orch".into(),
                uid,
                birth_identity: "epoch-1".into(),
            },
            GateOrphan {
                pid: 2,
                executable: "launchd-unrelated-agent".into(),
                uid,
                birth_identity: "epoch-2".into(),
            },
            GateOrphan {
                pid: 3,
                executable: "sleep".into(),
                uid: uid.saturating_add(1),
                birth_identity: "epoch-3".into(),
            },
        ];
        let (reportable, evidence_only) = partition_gate_orphans(&rows);
        assert_eq!(
            reportable.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            evidence_only.iter().map(|row| row.pid).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
