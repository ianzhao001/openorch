//! 门执行器（design/06 §3 机检门 · argv 直跑无 shell，design/08 §1）。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::Digest as _;
use wait_timeout::ChildExt;

use crate::{binding::CommandSpec, buildcache, cas};

/// Version of the candidate/merge gate-lane and workspace-full-permit contract.
pub const GATE_LANE_CONTRACT_V1: u32 = 1;

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
    #[link_name = "signal"]
    fn libc_signal(signal: i32, handler: usize) -> usize;
    fn getuid() -> u32;
}

#[cfg(unix)]
const SIG_DFL: usize = 0;
#[cfg(unix)]
const SIG_ERR: usize = usize::MAX;
#[cfg(unix)]
const GATE_CHILD_DEFAULT_SIGNALS: [i32; 3] = [1, 2, 15]; // SIGHUP, SIGINT, SIGTERM

pub const ORCH_GATE_FIXTURE_REGISTRY: &str = "ORCH_GATE_FIXTURE_REGISTRY";

const UNKNOWN_BIRTH_IDENTITY: &str = "-";

/// Preserve the complete caller tag while separating otherwise identical gate artifacts created
/// for the same task in different rounds.  Keeping the task tag first also preserves the existing
/// human-readable grouping of runtime logs.
pub fn round_scoped_log_tag(round: &str, tag: &str) -> String {
    format!("{tag}-round-{round}")
}

/// The lifecycle phase that owns one gate execution.
///
/// Keeping the set closed prevents callers from inventing free-form phase labels that later
/// consumers cannot compare or aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GatePhase {
    /// Seed-commit replay used to prove the task was red before implementation.
    RedReplay,
    /// Candidate worktree gates run while collecting an executor report.
    Collect,
    /// Gates run against the synthetic trial-merge tree.
    Trial,
    /// Root-verdict gates run against the fixed candidate tree.
    Root,
    /// Gates run against the immutable merge commit before recording the task.
    PostMerge,
    /// Gates rerun by an explicit record or merge recovery path.
    Recovery,
}

impl GatePhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::RedReplay => "red-replay",
            Self::Collect => "collect",
            Self::Trial => "trial",
            Self::Root => "root",
            Self::PostMerge => "postmerge",
            Self::Recovery => "recovery",
        }
    }
}

/// Build the unique log tag for one phase-scoped gate execution.
///
/// The existing round-scoped tag remains the outer namespace. Attempt, phase, and runtime-minted
/// run identity then ensure that a later execution can never truncate an earlier run's log.
pub fn phase_scoped_log_tag(
    round: &str,
    task_id: &str,
    attempt_id: &str,
    phase: GatePhase,
    gate_run_id: &str,
) -> String {
    let round_scoped = round_scoped_log_tag(round, task_id);
    format!(
        "{round_scoped}-{attempt_id}-{}-{gate_run_id}",
        phase.as_str()
    )
}

/// A fingerprint of the toolchain and execution environment used by a gate.
///
/// The two digests deliberately separate compiler identity from machine, sandbox, overlay, and
/// build-affecting environment so adoption policy can explain which dimension changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateEnvironmentFingerprint {
    /// SHA-256 over the resolved Cargo binary, Cargo version, verbose rustc version, and target.
    pub toolchain_digest: String,
    /// SHA-256 over OS build, architecture, sandbox class, overlay, and build environment.
    pub environment_digest: String,
}

/// Environment variables inherited by gate children that can alter Cargo or rustc semantics.
///
/// The list is closed so capture and comparison cannot silently disagree about which inherited
/// values matter. Unset variables are omitted while set values are hashed verbatim.
pub const BUILD_ENV_KEYS: &[&str] = &["RUSTFLAGS", "CARGO_HOME", "RUSTC_WRAPPER", "RUSTC"];

fn hash_fields<'a>(fields: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = sha2::Sha256::new();
    for field in fields {
        let bytes = field.as_bytes();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    hex::encode(digest.finalize())
}

/// Compute a toolchain digest from already captured raw facts.
///
/// Length-prefixing each field makes the mapping unambiguous and keeps the helper deterministic;
/// changing any compiler path, version, or target therefore changes the identity.
pub fn gate_toolchain_digest(
    cargo_realpath: &str,
    cargo_version: &str,
    rustc_vv: &str,
    target_triple: &str,
) -> String {
    hash_fields([cargo_realpath, cargo_version, rustc_vv, target_triple])
}

/// Compute an environment digest from already captured raw facts.
///
/// Build-environment pairs are sorted before hashing so process enumeration order cannot create a
/// false difference. The optional overlay remains distinct from both an empty overlay and no file.
pub fn gate_environment_digest(
    os_build: &str,
    arch: &str,
    sandbox_class: &str,
    machine_overlay: Option<&str>,
    build_env: &[(String, String)],
) -> String {
    let mut ordered_env = build_env.to_vec();
    ordered_env.sort();
    let overlay_presence = if machine_overlay.is_some() {
        "overlay:present"
    } else {
        "overlay:absent"
    };
    let mut fields = vec![
        os_build.to_string(),
        arch.to_string(),
        sandbox_class.to_string(),
        overlay_presence.to_string(),
        machine_overlay.unwrap_or_default().to_string(),
    ];
    for (key, value) in ordered_env {
        fields.push(key);
        fields.push(value);
    }
    hash_fields(fields.iter().map(String::as_str))
}

fn command_stdout(program: &str, args: &[&str], label: &str) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("采集 gate {label} 失败: {program} {args:?}"))?;
    if !output.status.success() {
        bail!(
            "采集 gate {label} 失败（{}）: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        bail!("采集 gate {label} 得到空输出");
    }
    Ok(stdout)
}

fn resolve_program(program: &str) -> Result<PathBuf> {
    let candidate = Path::new(program);
    let discovered = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join(program))
                    .find(|path| path.is_file())
            })
            .with_context(|| format!("gate 工具 {program} 不在 PATH"))?
    };
    fs::canonicalize(&discovered)
        .with_context(|| format!("解析 gate 工具 realpath 失败: {}", discovered.display()))
}

fn capture_os_build() -> Result<String> {
    if cfg!(target_os = "macos") {
        command_stdout("sw_vers", &["-buildVersion"], "OS build")
    } else {
        command_stdout("uname", &["-srv"], "OS build")
    }
}

fn capture_sandbox_class() -> String {
    const SANDBOX_KEYS: &[&str] = &[
        "ORCH_SANDBOX_CLASS",
        "CODEX_SANDBOX",
        "APP_SANDBOX_CONTAINER_ID",
    ];
    let observed = SANDBOX_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| format!("{key}={value}"))
        })
        .collect::<Vec<_>>();
    if observed.is_empty() {
        "none".to_string()
    } else {
        observed.join("|")
    }
}

/// Capture the real toolchain and environment used by a gate at the call site.
///
/// Cargo and rustc are executed rather than inferred from compile-time constants. Missing tools,
/// malformed version output, or unreadable overlay bytes are errors; capture never substitutes an
/// empty or constant digest because that would make unlike executions appear adoptable.
pub fn capture_gate_environment_fingerprint(root: &Path) -> Result<GateEnvironmentFingerprint> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let cargo_realpath = resolve_program(&cargo)?;
    let cargo_version = command_stdout(&cargo, &["-V"], "cargo version")?;
    let rustc_vv = command_stdout("rustc", &["-vV"], "rustc verbose version")?;
    let target_triple = rustc_vv
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("rustc -vV 缺 host target triple")?;

    let overlay_path = root.join(".orch/machine.yaml");
    let machine_overlay = match fs::read_to_string(&overlay_path) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 machine overlay 失败: {}", overlay_path.display()))
        }
    };
    let build_env = BUILD_ENV_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect::<Vec<_>>();
    Ok(GateEnvironmentFingerprint {
        toolchain_digest: gate_toolchain_digest(
            &cargo_realpath.display().to_string(),
            &cargo_version,
            &rustc_vv,
            target_triple,
        ),
        environment_digest: gate_environment_digest(
            &capture_os_build()?,
            std::env::consts::ARCH,
            &capture_sandbox_class(),
            machine_overlay.as_deref(),
            &build_env,
        ),
    })
}

/// Capture the exact Git tree visible to a gate without changing the worktree's real index.
///
/// A private temporary index includes tracked, staged, unstaged, and non-ignored untracked inputs;
/// this is required for pre-attempt seed oracles and synthetic trial merges that have no commit.
pub(crate) fn capture_gate_subject_tree(root: &Path, worktree: &Path) -> Result<String> {
    let scratch = root.join(".cowork-temp");
    fs::create_dir_all(&scratch)
        .with_context(|| format!("创建 gate subject scratch 失败: {}", scratch.display()))?;
    let temp_index = scratch.join(format!("gate-subject-index-{}", ulid::Ulid::new()));
    let captured = crate::gitx::snapshot_tree(root, worktree, &temp_index);
    let cleanup = match fs::remove_file(&temp_index) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "清理 gate subject temp index 失败: {}",
                temp_index.display()
            )
        }),
    };
    match (captured, cleanup) {
        (Ok(tree), Ok(())) => Ok(tree),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(error).context(format!(
            "gate subject 采集失败且清理失败: {cleanup_error:#}"
        )),
    }
}

/// Persist the canonical same-shaped fact for one completed production gate.
///
/// Log bytes are read only after the child is reaped, then bound by SHA-256 and length in the same
/// event as phase, subject tree, runtime run identity, toolchain, environment, exit, and duration.
pub(crate) fn record_gate_execution(
    root: &Path,
    round: &str,
    identity: crate::ledger::GateAuditIdentity<'_>,
    schema: (&str, &[&str]),
    phase: GatePhase,
    gate_run_id: &str,
    subject_tree_sha: &str,
    result: &GateResult,
    fingerprint: &GateEnvironmentFingerprint,
) -> Result<()> {
    identity.validate()?;
    const PAYLOAD_KEYS: &[&str] = &[
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
    ];
    if schema.0 != "GateExecuted" || schema.1 != PAYLOAD_KEYS {
        bail!("GateExecuted caller schema 与 canonical payload 不一致");
    }
    if gate_run_id.is_empty() || subject_tree_sha.is_empty() {
        bail!("GateExecuted gateRunId/subjectTreeSha 不能为空");
    }
    let log = fs::read(&result.log_path)
        .with_context(|| format!("读取 GateExecuted 原始日志失败: {}", result.log_path))?;
    let duration_ms =
        u64::try_from(result.duration_ms).context("GateExecuted durationMs 溢出 u64")?;
    let log_bytes = u64::try_from(log.len()).context("GateExecuted logBytes 溢出 u64")?;
    let log_sha256 = hex::encode(sha2::Sha256::digest(&log));
    crate::ledger::append(
        root,
        round,
        &[crate::ledger::event(
            schema.0,
            "runtime:orch",
            Some(identity.task_id()),
            Some(round),
            serde_json::json!({
                "commandRef": result.name,
                "phase": phase.as_str(),
                "gateRunId": gate_run_id,
                "exitCode": result.exit_code,
                "durationMs": duration_ms,
                "subjectTreeSha": subject_tree_sha,
                "logSha256": log_sha256,
                "logBytes": log_bytes,
                "toolchainDigest": fingerprint.toolchain_digest,
                "environmentDigest": fingerprint.environment_digest,
            }),
        )],
    )?;
    Ok(())
}

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GateRegistryReaping {
    pub processed: Vec<u32>,
    pub skipped_birth_mismatch: Vec<u32>,
    pub skipped_malformed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateRegistryBirthMismatch {
    pgid: u32,
    expected: String,
    observed: String,
}

impl std::fmt::Display for GateRegistryBirthMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "gate process group {} recorded anchor identity/birth mismatch: expected={} observed={}",
            self.pgid, self.expected, self.observed
        )
    }
}

impl std::error::Error for GateRegistryBirthMismatch {}

/// Reap every valid registered process group while reporting entries that could not be reaped.
/// Empty, torn, malformed, and stale-birth lines are evidence rather than sweep-wide failures.
pub fn reap_gate_fixture_registry_reporting(path: &Path) -> Result<GateRegistryReaping> {
    let mut control = SystemGateReapControl::new();
    reap_gate_fixture_registry_with_control(path, &mut control)
}

/// Compatibility projection for callers that only need the processed process-group ids.
pub fn reap_gate_fixture_registry(path: &Path) -> Result<Vec<u32>> {
    Ok(reap_gate_fixture_registry_reporting(path)?.processed)
}

fn reap_gate_fixture_registry_with_control(
    path: &Path,
    control: &mut dyn GateReapControl,
) -> Result<GateRegistryReaping> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GateRegistryReaping::default())
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read gate fixture registry failed: {}", path.display()))
        }
    };
    let mut report = GateRegistryReaping::default();
    let mut failures = Vec::new();
    for (index, raw_line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if raw_line.last() != Some(&b'\n') {
            report.skipped_malformed += 1;
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
            report.skipped_malformed += 1;
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
            report.skipped_malformed += 1;
            eprintln!(
                "gate fixture registry {} skipped malformed line {}",
                path.display(),
                index + 1
            );
            continue;
        };
        report.processed.push(pgid);
        if let Err(error) = reap_gate_process_group_kernel(
            pgid,
            recorded_birth,
            ReapPolicy::RegisteredFixture,
            control,
        ) {
            if error.downcast_ref::<GateRegistryBirthMismatch>().is_some() {
                report.skipped_birth_mismatch.push(pgid);
                eprintln!(
                    "gate fixture registry {} skipped pgid {} birth mismatch: {error:#}",
                    path.display(),
                    pgid
                );
                continue;
            }
            failures.push(format!("pgid {pgid}: {error:#}"));
        }
    }
    if report.skipped_malformed > 0 {
        eprintln!(
            "gate fixture registry {} skipped {} empty/torn/malformed line(s)",
            path.display(),
            report.skipped_malformed
        );
    }
    if !failures.is_empty() {
        bail!(
            "gate fixture registry cleanup failed after visiting every group: {}",
            failures.join("; ")
        );
    }
    Ok(report)
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
            return Err(GateRegistryBirthMismatch {
                pgid,
                expected: expected.to_owned(),
                observed: observed.birth_identity.clone(),
            }
            .into());
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

fn reap_workspace_full_process_group(pgid: u32, birth_identity: &str) -> Result<()> {
    let mut control = SystemGateReapControl::new();
    reap_gate_process_group_kernel(
        pgid,
        Some(birth_identity),
        ReapPolicy::DirectGroup,
        &mut control,
    )
}

#[doc(hidden)]
pub mod b249_contract {
    pub use super::{
        reap_gate_process_group, reap_gate_process_group_with_control, GateGroupMember,
        GateGroupObservation, GateReapControl, GATE_REAP_POLL_INTERVAL, GATE_REAP_TOTAL_BUDGET,
    };
    pub use crate::gitx::{canonical_worktree_common_dir, worktree_add, worktree_add_detached};
}

const WORKSPACE_FULL_LOCK_FILE: &str = "orch-workspace-full-v1.lock";
const WORKSPACE_FULL_OWNER_FILE: &str = "orch-workspace-full-v1.owner.json";
const WORKSPACE_FULL_WAIT_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceFullOwnerV1 {
    schema_version: u32,
    repo_identity_sha256: String,
    gate_run_id: String,
    pid: u32,
    birth_identity: String,
    action: String,
    token: String,
    child: WorkspaceFullChildStateV1,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "state",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum WorkspaceFullChildStateV1 {
    Unspawned,
    Spawning {
        fixture_registry: String,
    },
    Running {
        pgid: u32,
        birth_identity: String,
        fixture_registry: String,
    },
    GoneBeforeBirth {
        pgid: u32,
        fixture_registry: String,
    },
    Converged {
        pgid: u32,
        birth_identity: String,
        fixture_registry: String,
    },
    ConvergedGoneBeforeBirth {
        pgid: u32,
        fixture_registry: String,
    },
}

impl WorkspaceFullOwnerV1 {
    fn validate(&self) -> Result<()> {
        if self.schema_version != GATE_LANE_CONTRACT_V1 {
            bail!("workspace full permit owner schemaVersion 必须为 1");
        }
        for (label, value) in [
            ("repoIdentitySha256", self.repo_identity_sha256.as_str()),
            ("gateRunId", self.gate_run_id.as_str()),
            ("birthIdentity", self.birth_identity.as_str()),
            ("action", self.action.as_str()),
            ("token", self.token.as_str()),
        ] {
            if value.trim().is_empty() || value.trim() != value {
                bail!("workspace full permit owner {label} 为空或含首尾空白");
            }
        }
        if self.pid == 0 {
            bail!("workspace full permit owner pid 必须为正数");
        }
        if self.repo_identity_sha256.len() != 64
            || !self
                .repo_identity_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("workspace full permit owner repoIdentitySha256 非 lowercase hex64");
        }
        match &self.child {
            WorkspaceFullChildStateV1::Unspawned => {}
            WorkspaceFullChildStateV1::Spawning { fixture_registry } => {
                if fixture_registry.trim().is_empty()
                    || fixture_registry.trim() != fixture_registry
                    || !Path::new(fixture_registry).is_absolute()
                {
                    bail!("workspace full permit spawning registry 非 canonical");
                }
            }
            WorkspaceFullChildStateV1::Running {
                pgid,
                birth_identity,
                fixture_registry,
            }
            | WorkspaceFullChildStateV1::Converged {
                pgid,
                birth_identity,
                fixture_registry,
            } => {
                if *pgid == 0
                    || birth_identity.trim().is_empty()
                    || birth_identity.trim() != birth_identity
                    || fixture_registry.trim().is_empty()
                    || fixture_registry.trim() != fixture_registry
                    || !Path::new(fixture_registry).is_absolute()
                {
                    bail!("workspace full permit child identity/registry 非 canonical");
                }
            }
            WorkspaceFullChildStateV1::GoneBeforeBirth {
                pgid,
                fixture_registry,
            }
            | WorkspaceFullChildStateV1::ConvergedGoneBeforeBirth {
                pgid,
                fixture_registry,
            } => {
                if *pgid == 0
                    || fixture_registry.trim().is_empty()
                    || fixture_registry.trim() != fixture_registry
                    || !Path::new(fixture_registry).is_absolute()
                {
                    bail!("workspace full permit gone-child proof/registry 非 canonical");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct WorkspaceFullPaths {
    scope: PathBuf,
    repo_identity_sha256: String,
    directory: PathBuf,
    lock: PathBuf,
    owner: PathBuf,
}

fn workspace_full_paths(root: &Path) -> Result<WorkspaceFullPaths> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("解析 workspace full permit root 失败: {}", root.display()))?;
    let scope = crate::gitx::canonical_worktree_common_dir(&canonical_root).with_context(|| {
        format!(
            "workspace full permit 要求可解析的 repository common-dir: {}",
            canonical_root.display()
        )
    })?;
    let directory = scope.clone();
    let mut repo_digest = sha2::Sha256::new();
    repo_digest.update(b"orch-workspace-full-repo-v1\0");
    repo_digest.update(scope.to_string_lossy().as_bytes());
    let repo_identity_sha256 = hex::encode(repo_digest.finalize());
    Ok(WorkspaceFullPaths {
        scope,
        repo_identity_sha256,
        lock: directory.join(WORKSPACE_FULL_LOCK_FILE),
        owner: directory.join(WORKSPACE_FULL_OWNER_FILE),
        directory,
    })
}

fn ensure_regular_or_missing(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!(
                "{label} 必须是 regular non-symlink file: {}",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("检查 {label} 失败: {}", path.display())),
    }
}

fn with_workspace_full_state_lock<T>(
    paths: &WorkspaceFullPaths,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    ensure_regular_or_missing(&paths.lock, "workspace full permit lock")?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&paths.lock)
        .with_context(|| {
            format!(
                "打开 workspace full permit lock 失败: {}",
                paths.lock.display()
            )
        })?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write().with_context(|| {
        format!(
            "等待 workspace full permit 短锁失败: {}",
            paths.lock.display()
        )
    })?;
    action()
}

fn read_workspace_full_owner(path: &Path) -> Result<Option<WorkspaceFullOwnerV1>> {
    ensure_regular_or_missing(path, "workspace full permit owner")?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("读取 workspace full permit owner 失败: {}", path.display())
            })
        }
    };
    let owner: WorkspaceFullOwnerV1 = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "workspace full permit owner 非 canonical JSON: {}",
            path.display()
        )
    })?;
    owner.validate()?;
    Ok(Some(owner))
}

fn write_workspace_full_owner(
    paths: &WorkspaceFullPaths,
    owner: &WorkspaceFullOwnerV1,
) -> Result<()> {
    owner.validate()?;
    ensure_regular_or_missing(&paths.owner, "workspace full permit owner")?;
    let temporary = paths.directory.join(format!(
        ".{WORKSPACE_FULL_OWNER_FILE}.tmp-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "创建 workspace full permit owner temp 失败: {}",
                    temporary.display()
                )
            })?;
        serde_json::to_writer(&mut file, owner)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &paths.owner).with_context(|| {
            format!(
                "发布 workspace full permit owner 失败: {}",
                paths.owner.display()
            )
        })?;
        File::open(&paths.directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn workspace_full_owner_is_live(owner: &WorkspaceFullOwnerV1) -> Result<bool> {
    Ok(crate::wake::managed_process_birth_identity(owner.pid)?
        .is_some_and(|birth| birth == owner.birth_identity))
}

fn workspace_full_group_converged(pgid: u32, expected_birth: &str) -> Result<bool> {
    match crate::wake::observe_exact_gate_group_members(pgid) {
        Ok(observation) => workspace_full_observation_converged(pgid, expected_birth, observation),
        Err(mut last_error) => {
            // A disappearing leader can leave one transient opaque process-table
            // sample. Retry only this read-only census; identity mismatches in
            // the successful-observation path are never retried or softened.
            for _ in 0..9 {
                std::thread::sleep(Duration::from_millis(5));
                match crate::wake::observe_exact_gate_group_members(pgid) {
                    Ok(observation) => {
                        return workspace_full_observation_converged(
                            pgid,
                            expected_birth,
                            observation,
                        )
                    }
                    Err(error) => last_error = error,
                }
            }
            Err(last_error).context("workspace full permit child group census failed")
        }
    }
}

fn workspace_full_gone_proof_converged(pgid: u32) -> Result<bool> {
    for attempt in 0..10 {
        match crate::wake::observe_exact_gate_group_members(pgid) {
            Ok(None) => {}
            Ok(Some(members)) => {
                bail!(
                    "workspace full permit gone-before-birth pgid {pgid} reappeared with {} member(s); refusing unanchored convergence",
                    members.len()
                )
            }
            Err(error) => {
                return Err(error).context("workspace full permit gone-before-birth census failed")
            }
        }
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Ok(true)
}

fn workspace_full_observation_converged(
    pgid: u32,
    expected_birth: &str,
    observation: Option<Vec<(u32, String, bool)>>,
) -> Result<bool> {
    let Some(members) = observation else {
        return Ok(true);
    };
    if members.is_empty()
        || members
            .iter()
            .any(|(pid, birth, _)| *pid == 0 || birth.is_empty())
    {
        bail!("workspace full permit child group census ambiguous");
    }
    if let Some((_, observed_birth, _)) = members.iter().find(|(pid, _, _)| *pid == pgid) {
        if observed_birth != expected_birth {
            bail!(
                "workspace full permit child birth mismatch: pgid={pgid} expected={expected_birth} observed={observed_birth}"
            );
        }
    }
    Ok(members.iter().all(|(_, _, zombie)| *zombie))
}

fn workspace_full_fixture_registry_converged(path: &Path) -> Result<bool> {
    ensure_regular_or_missing(path, "workspace full permit fixture registry")?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "workspace full permit fixture registry missing after durable declaration: {}",
                path.display()
            )
        }
        Err(error) => {
            return Err(error).context("读取 workspace full permit fixture registry 失败")
        }
    };
    if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
        bail!("workspace full permit fixture registry 含 torn line");
    }
    if bytes.is_empty() {
        return Ok(true);
    }
    for raw in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        if raw.is_empty() {
            bail!("workspace full permit fixture registry 含 empty line");
        }
        let line =
            std::str::from_utf8(raw).context("workspace full permit fixture registry 非 UTF-8")?;
        let fields = line.trim_end_matches('\r').split('\t').collect::<Vec<_>>();
        if fields.len() != 3 {
            bail!("workspace full permit fixture registry line 非 exact 三字段");
        }
        let pid = fields[0]
            .parse::<u32>()
            .context("workspace full permit fixture pid 非 u32")?;
        let pgid = fields[1]
            .parse::<u32>()
            .context("workspace full permit fixture pgid 非 u32")?;
        let birth = fields[2];
        if pid == 0 || pgid == 0 || birth.is_empty() || birth == UNKNOWN_BIRTH_IDENTITY {
            bail!("workspace full permit fixture registry identity 不完整");
        }
        if !workspace_full_group_converged(pgid, birth)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn workspace_full_child_converged(child: &WorkspaceFullChildStateV1) -> Result<bool> {
    let (group_converged, fixture_registry) = match child {
        WorkspaceFullChildStateV1::Unspawned => {
            bail!("dead workspace full owner lacks child convergence proof")
        }
        WorkspaceFullChildStateV1::Spawning { .. } => {
            bail!("dead workspace full owner stopped during child spawn; exact pgid unknown")
        }
        WorkspaceFullChildStateV1::Running {
            pgid,
            birth_identity,
            fixture_registry,
        }
        | WorkspaceFullChildStateV1::Converged {
            pgid,
            birth_identity,
            fixture_registry,
        } => (
            workspace_full_group_converged(*pgid, birth_identity)?,
            fixture_registry.as_str(),
        ),
        WorkspaceFullChildStateV1::GoneBeforeBirth {
            pgid,
            fixture_registry,
        }
        | WorkspaceFullChildStateV1::ConvergedGoneBeforeBirth {
            pgid,
            fixture_registry,
        } => (
            workspace_full_gone_proof_converged(*pgid)?,
            fixture_registry.as_str(),
        ),
    };
    Ok(group_converged && workspace_full_fixture_registry_converged(Path::new(fixture_registry))?)
}

fn workspace_full_owner_reclaimable(owner: &WorkspaceFullOwnerV1) -> Result<bool> {
    if workspace_full_owner_is_live(owner)? {
        return Ok(false);
    }
    match &owner.child {
        WorkspaceFullChildStateV1::Unspawned => Ok(true),
        child => workspace_full_child_converged(child),
    }
}

fn workspace_full_local_owners() -> &'static Mutex<HashMap<PathBuf, String>> {
    static OWNERS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct WorkspaceFullLocalClaim {
    scope: PathBuf,
    token: String,
    retained: bool,
}

impl WorkspaceFullLocalClaim {
    fn begin(scope: &Path, token: &str) -> Result<Self> {
        let mut owners = workspace_full_local_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.contains_key(scope) {
            bail!(
                "workspace full permit 已由本进程持有；同进程重入必须使用 holder 派生的 reentry handle"
            );
        }
        owners.insert(scope.to_path_buf(), token.to_string());
        Ok(Self {
            scope: scope.to_path_buf(),
            token: token.to_string(),
            retained: false,
        })
    }

    fn retain(mut self) {
        self.retained = true;
    }
}

impl Drop for WorkspaceFullLocalClaim {
    fn drop(&mut self) {
        if self.retained {
            return;
        }
        let mut owners = workspace_full_local_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.get(&self.scope) == Some(&self.token) {
            owners.remove(&self.scope);
        }
    }
}

fn remove_workspace_full_local_owner(scope: &Path, token: &str) {
    let mut owners = workspace_full_local_owners()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if owners.get(scope).is_some_and(|current| current == token) {
        owners.remove(scope);
    }
}

fn same_workspace_full_authority(
    left: &WorkspaceFullOwnerV1,
    right: &WorkspaceFullOwnerV1,
) -> bool {
    left.schema_version == right.schema_version
        && left.repo_identity_sha256 == right.repo_identity_sha256
        && left.gate_run_id == right.gate_run_id
        && left.pid == right.pid
        && left.birth_identity == right.birth_identity
        && left.action == right.action
        && left.token == right.token
}

fn ensure_workspace_full_current_process(authority: &WorkspaceFullOwnerV1) -> Result<()> {
    let pid = std::process::id();
    if authority.pid != pid {
        bail!(
            "workspace full permit authority belongs to pid {}, current pid is {pid}",
            authority.pid
        );
    }
    let birth_identity = crate::wake::managed_process_birth_identity(pid)?
        .context("current workspace full permit process lacks birth identity")?;
    if birth_identity != authority.birth_identity {
        bail!("workspace full permit current process birth identity drift");
    }
    Ok(())
}

#[derive(Debug)]
struct WorkspaceFullChildLease {
    paths: WorkspaceFullPaths,
    authority: WorkspaceFullOwnerV1,
    pgid: u32,
    epoch: WorkspaceFullChildEpoch,
    fixture_registry: String,
}

#[derive(Debug, Clone)]
enum WorkspaceFullChildEpoch {
    Anchored { birth_identity: String },
    GoneBeforeBirth,
}

impl WorkspaceFullChildLease {
    fn running_state(&self) -> WorkspaceFullChildStateV1 {
        match &self.epoch {
            WorkspaceFullChildEpoch::Anchored { birth_identity } => {
                WorkspaceFullChildStateV1::Running {
                    pgid: self.pgid,
                    birth_identity: birth_identity.clone(),
                    fixture_registry: self.fixture_registry.clone(),
                }
            }
            WorkspaceFullChildEpoch::GoneBeforeBirth => {
                WorkspaceFullChildStateV1::GoneBeforeBirth {
                    pgid: self.pgid,
                    fixture_registry: self.fixture_registry.clone(),
                }
            }
        }
    }

    fn converged_state(&self) -> WorkspaceFullChildStateV1 {
        match &self.epoch {
            WorkspaceFullChildEpoch::Anchored { birth_identity } => {
                WorkspaceFullChildStateV1::Converged {
                    pgid: self.pgid,
                    birth_identity: birth_identity.clone(),
                    fixture_registry: self.fixture_registry.clone(),
                }
            }
            WorkspaceFullChildEpoch::GoneBeforeBirth => {
                WorkspaceFullChildStateV1::ConvergedGoneBeforeBirth {
                    pgid: self.pgid,
                    fixture_registry: self.fixture_registry.clone(),
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct WorkspaceFullExecutionContext {
    scope: PathBuf,
    authority: WorkspaceFullOwnerV1,
}

std::thread_local! {
    static WORKSPACE_FULL_EXECUTION: std::cell::RefCell<Option<WorkspaceFullExecutionContext>> =
        const { std::cell::RefCell::new(None) };
}

struct WorkspaceFullExecutionGuard {
    scope: PathBuf,
    token: String,
}

impl WorkspaceFullExecutionGuard {
    fn begin(paths: &WorkspaceFullPaths, authority: &WorkspaceFullOwnerV1) -> Result<Self> {
        ensure_workspace_full_current_process(authority)?;
        with_workspace_full_state_lock(paths, || {
            let observed = read_workspace_full_owner(&paths.owner)?
                .context("workspace full permit execution 缺 owner")?;
            if !same_workspace_full_authority(&observed, authority) {
                bail!("workspace full permit execution owner authority 漂移");
            }
            match &observed.child {
                WorkspaceFullChildStateV1::Unspawned => {}
                WorkspaceFullChildStateV1::Converged { .. }
                | WorkspaceFullChildStateV1::ConvergedGoneBeforeBirth { .. }
                    if workspace_full_child_converged(&observed.child)? => {}
                _ => bail!("workspace full permit execution 遇到未收敛 child/spawn"),
            }
            Ok(())
        })?;
        let owners = workspace_full_local_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.get(&paths.scope) != Some(&authority.token) {
            bail!("workspace full permit execution process-local authority 漂移");
        }
        drop(owners);
        WORKSPACE_FULL_EXECUTION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_some() {
                bail!("workspace full permit execution capability 已在本线程使用");
            }
            *slot = Some(WorkspaceFullExecutionContext {
                scope: paths.scope.clone(),
                authority: authority.clone(),
            });
            Ok(Self {
                scope: paths.scope.clone(),
                token: authority.token.clone(),
            })
        })
    }
}

impl Drop for WorkspaceFullExecutionGuard {
    fn drop(&mut self) {
        WORKSPACE_FULL_EXECUTION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.as_ref().is_some_and(|context| {
                context.scope == self.scope && context.authority.token == self.token
            }) {
                *slot = None;
            }
        });
    }
}

#[derive(Debug)]
struct WorkspaceFullChildPreparation {
    paths: WorkspaceFullPaths,
    authority: WorkspaceFullOwnerV1,
    fixture_registry: String,
}

fn active_workspace_full_binding(
    root: &Path,
) -> Result<Option<(WorkspaceFullPaths, WorkspaceFullOwnerV1)>> {
    let Some(context) = WORKSPACE_FULL_EXECUTION.with(|slot| slot.borrow().clone()) else {
        return Ok(None);
    };
    let paths = workspace_full_paths(root)?;
    if paths.scope != context.scope {
        bail!("workspace full permit execution capability repo identity 漂移");
    }
    Ok(Some((paths, context.authority)))
}

fn canonical_registry_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().context("gate fixture registry 缺 parent")?;
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent).context("解析 gate fixture registry parent 失败")?;
    let name = path
        .file_name()
        .context("gate fixture registry 缺 filename")?;
    let canonical = parent.join(name);
    ensure_regular_or_missing(&canonical, "workspace full permit fixture registry")?;
    Ok(canonical)
}

fn initialize_empty_registry(canonical: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&canonical)
        .with_context(|| {
            format!(
                "初始化 workspace full permit fixture registry 失败: {}",
                canonical.display()
            )
        })?;
    file.sync_all()?;
    File::open(
        canonical
            .parent()
            .context("workspace full permit fixture registry 缺 canonical parent")?,
    )?
    .sync_all()?;
    Ok(())
}

fn prepare_workspace_full_child_if_owned(
    workdir: &Path,
    fixture_registry: &Path,
) -> Result<Option<WorkspaceFullChildPreparation>> {
    let Some((paths, authority)) = active_workspace_full_binding(workdir)? else {
        return Ok(None);
    };
    let fixture_registry = canonical_registry_path(fixture_registry)?;
    with_workspace_full_state_lock(&paths, || {
        let mut owner = read_workspace_full_owner(&paths.owner)?
            .context("workspace full permit prepare child 缺 owner")?;
        if !same_workspace_full_authority(&owner, &authority)
            || owner.pid != std::process::id()
            || owner.repo_identity_sha256 != paths.repo_identity_sha256
        {
            bail!("workspace full permit prepare child owner authority 漂移");
        }
        match &owner.child {
            WorkspaceFullChildStateV1::Unspawned => {}
            WorkspaceFullChildStateV1::Converged { .. }
            | WorkspaceFullChildStateV1::ConvergedGoneBeforeBirth { .. }
                if workspace_full_child_converged(&owner.child)? => {}
            _ => bail!("workspace full permit 已有未收敛 child/spawn，拒绝并发重入"),
        }
        initialize_empty_registry(&fixture_registry)?;
        let fixture_registry = fixture_registry.to_string_lossy().into_owned();
        owner.child = WorkspaceFullChildStateV1::Spawning {
            fixture_registry: fixture_registry.clone(),
        };
        write_workspace_full_owner(&paths, &owner)
    })?;
    Ok(Some(WorkspaceFullChildPreparation {
        paths,
        authority,
        fixture_registry: fixture_registry.to_string_lossy().into_owned(),
    }))
}

fn cancel_workspace_full_child_preparation(
    preparation: &WorkspaceFullChildPreparation,
) -> Result<()> {
    with_workspace_full_state_lock(&preparation.paths, || {
        let mut owner = read_workspace_full_owner(&preparation.paths.owner)?
            .context("workspace full permit cancel spawn 缺 owner")?;
        if !same_workspace_full_authority(&owner, &preparation.authority)
            || owner.child
                != (WorkspaceFullChildStateV1::Spawning {
                    fixture_registry: preparation.fixture_registry.clone(),
                })
        {
            bail!("workspace full permit cancel spawn identity 漂移");
        }
        owner.child = WorkspaceFullChildStateV1::Unspawned;
        write_workspace_full_owner(&preparation.paths, &owner)
    })
}

fn classify_workspace_full_child_sample(
    managed_birth: Option<String>,
    leader_birth: Option<String>,
    child_exited: bool,
    gone_proven: bool,
) -> Result<Option<WorkspaceFullChildEpoch>> {
    if let (Some(managed), Some(leader)) = (&managed_birth, &leader_birth) {
        if managed != leader {
            bail!(
                "workspace full permit gate child birth sources disagree: managed={managed} leader={leader}"
            );
        }
    }
    if let Some(birth_identity) = leader_birth.or(managed_birth) {
        return Ok(Some(WorkspaceFullChildEpoch::Anchored { birth_identity }));
    }
    if child_exited && gone_proven {
        return Ok(Some(WorkspaceFullChildEpoch::GoneBeforeBirth));
    }
    Ok(None)
}

fn observe_workspace_full_child_epoch(
    child: &mut std::process::Child,
) -> Result<WorkspaceFullChildEpoch> {
    let pgid = child.id();
    let mut last_reason = String::from("no process-table observation");
    for attempt in 0..10 {
        let managed_birth = match crate::wake::managed_process_birth_identity(pgid) {
            Ok(Some(birth_identity)) if !birth_identity.is_empty() => Some(birth_identity),
            Ok(Some(_)) | Ok(None) => None,
            Err(_) => None,
        };
        let leader_birth = match crate::wake::observe_exact_gate_group_members(pgid) {
            Ok(Some(members)) => {
                let birth = members
                    .into_iter()
                    .find(|(pid, _, _)| *pid == pgid)
                    .map(|(_, birth, _)| birth)
                    .filter(|birth| !birth.is_empty());
                birth
            }
            Ok(None) => None,
            Err(_) => None,
        };
        let child_exited = match child.try_wait() {
            Ok(None) => false,
            Ok(Some(_)) => true,
            Err(error) => {
                last_reason = format!("child exit observation failed: {error}");
                if attempt < 9 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                continue;
            }
        };
        let has_birth = managed_birth.is_some() || leader_birth.is_some();
        let gone_proven = if child_exited && !has_birth {
            workspace_full_gone_proof_converged(pgid)?
        } else {
            false
        };
        if let Some(epoch) = classify_workspace_full_child_sample(
            managed_birth,
            leader_birth,
            child_exited,
            gone_proven,
        )? {
            // The leader may exit between the read-only birth observations and try_wait while a
            // descendant keeps the original group alive. The classifier deliberately retains an
            // authenticated birth in that case, authorizing exact anchored cleanup.
            return Ok(epoch);
        }
        if child_exited {
            last_reason = "exited child lacks a stable absent group proof".to_string();
        } else {
            last_reason =
                "child handle is live without a census-bound leader birth identity".to_string();
        }
        if attempt < 9 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    bail!("workspace full permit 无法捕获 exact gate child epoch: pgid={pgid} reason={last_reason}")
}

fn capture_workspace_full_child(
    preparation: &WorkspaceFullChildPreparation,
    child: &mut std::process::Child,
) -> Result<WorkspaceFullChildLease> {
    Ok(WorkspaceFullChildLease {
        paths: preparation.paths.clone(),
        authority: preparation.authority.clone(),
        pgid: child.id(),
        epoch: observe_workspace_full_child_epoch(child)?,
        fixture_registry: preparation.fixture_registry.clone(),
    })
}

fn persist_workspace_full_child(
    preparation: &WorkspaceFullChildPreparation,
    lease: &WorkspaceFullChildLease,
) -> Result<()> {
    if lease.paths.scope != preparation.paths.scope
        || !same_workspace_full_authority(&lease.authority, &preparation.authority)
        || lease.fixture_registry != preparation.fixture_registry
    {
        bail!("workspace full permit child lease/preparation authority 漂移");
    }
    let running = lease.running_state();
    with_workspace_full_state_lock(&preparation.paths, || {
        let mut owner = read_workspace_full_owner(&preparation.paths.owner)?
            .context("workspace full permit bind child 缺 owner")?;
        if !same_workspace_full_authority(&owner, &preparation.authority) {
            bail!("workspace full permit bind child owner authority 漂移");
        }
        if owner.child == running {
            return write_workspace_full_owner(&preparation.paths, &owner);
        }
        if owner.child
            != (WorkspaceFullChildStateV1::Spawning {
                fixture_registry: preparation.fixture_registry.clone(),
            })
        {
            bail!("workspace full permit bind child state identity 漂移");
        }
        owner.child = running;
        write_workspace_full_owner(&preparation.paths, &owner)
    })
}

fn mark_workspace_full_child_converged(lease: &WorkspaceFullChildLease) -> Result<()> {
    let running = lease.running_state();
    if !workspace_full_child_converged(&running)? {
        bail!("workspace full permit child/fixture registry 尚未收敛");
    }
    with_workspace_full_state_lock(&lease.paths, || {
        let mut owner = read_workspace_full_owner(&lease.paths.owner)?
            .context("workspace full permit converge child 缺 owner")?;
        if !same_workspace_full_authority(&owner, &lease.authority) || owner.child != running {
            bail!("workspace full permit converge child identity 漂移");
        }
        owner.child = lease.converged_state();
        write_workspace_full_owner(&lease.paths, &owner)
    })
}

fn reap_bound_workspace_full_child(lease: &WorkspaceFullChildLease) -> Result<()> {
    match &lease.epoch {
        WorkspaceFullChildEpoch::Anchored { birth_identity } => {
            reap_workspace_full_process_group(lease.pgid, birth_identity)
        }
        WorkspaceFullChildEpoch::GoneBeforeBirth => {
            if workspace_full_gone_proof_converged(lease.pgid)? {
                Ok(())
            } else {
                bail!("workspace full permit gone-before-birth child 尚未收敛")
            }
        }
    }
}

fn recover_workspace_full_child_after_bind_failure(
    preparation: &WorkspaceFullChildPreparation,
    lease: &WorkspaceFullChildLease,
    child: &mut std::process::Child,
) -> Result<()> {
    let persist = persist_workspace_full_child(preparation, lease);
    match persist {
        Ok(()) => cleanup_bound_workspace_full_child(lease, child),
        Err(persist_error) => {
            let cleanup = cleanup_unbound_workspace_full_child(preparation, child);
            match cleanup {
                Ok(()) => Err(persist_error).context(
                    "workspace full permit bind-error recovery could not publish the child; the exact group was safely converged",
                ),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "workspace full permit bind-error recovery could not publish the child: {persist_error:#}; unbound cleanup also failed: {cleanup_error:#}"
                )),
            }
        }
    }
}

fn push_workspace_full_cleanup_error(
    errors: &mut Vec<String>,
    stage: &str,
    result: Result<()>,
) {
    if let Err(error) = result {
        errors.push(format!("{stage}: {error:#}"));
    }
}

fn finish_workspace_full_cleanup(errors: Vec<String>) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "workspace full permit child cleanup failed after every cleanup arm ran: {}",
            errors.join("; ")
        )
    }
}

fn wait_workspace_full_child(child: &mut std::process::Child) -> Result<()> {
    child
        .wait()
        .map(|_| ())
        .context("workspace full permit gate child wait failed")
}

fn cleanup_bound_workspace_full_child(
    lease: &WorkspaceFullChildLease,
    child: &mut std::process::Child,
) -> Result<()> {
    let mut errors = Vec::new();
    push_workspace_full_cleanup_error(
        &mut errors,
        "primary process-group reap",
        reap_bound_workspace_full_child(lease),
    );
    push_workspace_full_cleanup_error(
        &mut errors,
        "fixture-registry reap",
        reap_gate_fixture_registry(Path::new(&lease.fixture_registry)).map(|_| ()),
    );
    push_workspace_full_cleanup_error(
        &mut errors,
        "child wait",
        wait_workspace_full_child(child),
    );
    push_workspace_full_cleanup_error(
        &mut errors,
        "durable convergence",
        mark_workspace_full_child_converged(lease),
    );
    finish_workspace_full_cleanup(errors)
}

fn finish_gate_child_cleanup_after_reaps(
    full_child: Option<&WorkspaceFullChildLease>,
    child: &mut std::process::Child,
    primary_reap: Result<()>,
    registry_reap: Result<()>,
) -> Result<()> {
    let mut errors = Vec::new();
    push_workspace_full_cleanup_error(&mut errors, "process-group reap", primary_reap);
    push_workspace_full_cleanup_error(&mut errors, "fixture-registry reap", registry_reap);
    push_workspace_full_cleanup_error(
        &mut errors,
        "child wait",
        wait_workspace_full_child(child),
    );
    if let Some(binding) = full_child {
        push_workspace_full_cleanup_error(
            &mut errors,
            "durable convergence",
            mark_workspace_full_child_converged(binding),
        );
    }
    finish_workspace_full_cleanup(errors)
}

fn cleanup_unbound_workspace_full_child(
    preparation: &WorkspaceFullChildPreparation,
    child: &mut std::process::Child,
) -> Result<()> {
    let pgid = child.id();
    let registry = Path::new(&preparation.fixture_registry);
    let mut errors = Vec::new();
    push_workspace_full_cleanup_error(
        &mut errors,
        "unbound process-group reap",
        reap_gate_process_group(pgid),
    );
    push_workspace_full_cleanup_error(
        &mut errors,
        "unbound fixture-registry reap",
        reap_gate_fixture_registry(registry).map(|_| ()),
    );
    push_workspace_full_cleanup_error(
        &mut errors,
        "unbound child wait",
        wait_workspace_full_child(child),
    );

    let group_converged = match workspace_full_gone_proof_converged(pgid) {
        Ok(true) => true,
        Ok(false) => false,
        Err(error) => {
            errors.push(format!("unbound process-group convergence: {error:#}"));
            false
        }
    };
    let registry_converged = match workspace_full_fixture_registry_converged(registry) {
        Ok(converged) => converged,
        Err(error) => {
            errors.push(format!("unbound fixture-registry convergence: {error:#}"));
            false
        }
    };
    if group_converged && registry_converged {
        push_workspace_full_cleanup_error(
            &mut errors,
            "cancel durable spawning state",
            cancel_workspace_full_child_preparation(preparation),
        );
    } else {
        errors.push(
            "unbound child or fixture registry did not converge; durable Spawning state retained"
                .to_string(),
        );
    }
    finish_workspace_full_cleanup(errors)
}

/// Repository-wide capability which serializes a full gate across all linked worktrees.
///
/// Acquisition and release mutate the owner record under a dedicated short lock. The long-running
/// gate holds only this unforgeable Rust capability: it holds neither the dedicated state lock nor
/// `ledger.lock`. The owner record also binds repository identity and the current gate child
/// process-group/fixture registry. A dead parent is reclaimed only after every exact child epoch
/// converges; missing child proof, live groups, and birth drift fail closed.
#[derive(Debug)]
pub struct WorkspaceFullPermit {
    paths: WorkspaceFullPaths,
    owner: WorkspaceFullOwnerV1,
    active: bool,
    release_on_drop: bool,
}

impl WorkspaceFullPermit {
    /// Wait for and acquire the full-gate capability for one repository.
    ///
    /// `gate_run_id`, the current PID and birth identity, and `action` are persisted together.
    /// Independent acquisition by the same process is rejected; nested code must receive a
    /// [`WorkspaceFullPermitHandle`] derived from the returned holder.
    pub fn acquire(root: &Path, gate_run_id: &str, action: &str) -> Result<Self> {
        if gate_run_id.trim().is_empty() || gate_run_id.trim() != gate_run_id {
            bail!("workspace full permit gateRunId 为空或含首尾空白");
        }
        if action.trim().is_empty() || action.trim() != action {
            bail!("workspace full permit action 为空或含首尾空白");
        }
        let paths = workspace_full_paths(root)?;
        let pid = std::process::id();
        let birth_identity = crate::wake::managed_process_birth_identity(pid)?
            .context("当前 workspace full permit owner 缺 birth identity")?;
        if birth_identity.is_empty() {
            bail!("当前 workspace full permit owner birth identity 为空");
        }
        let token = ulid::Ulid::new().to_string();
        let local_claim = WorkspaceFullLocalClaim::begin(&paths.scope, &token)?;
        let desired = WorkspaceFullOwnerV1 {
            schema_version: GATE_LANE_CONTRACT_V1,
            repo_identity_sha256: paths.repo_identity_sha256.clone(),
            gate_run_id: gate_run_id.to_string(),
            pid,
            birth_identity,
            action: action.to_string(),
            token,
            child: WorkspaceFullChildStateV1::Unspawned,
        };
        desired.validate()?;

        loop {
            let observed =
                with_workspace_full_state_lock(&paths, || read_workspace_full_owner(&paths.owner))?;
            match observed {
                None => {
                    let claimed = with_workspace_full_state_lock(&paths, || {
                        if read_workspace_full_owner(&paths.owner)?.is_some() {
                            return Ok(false);
                        }
                        write_workspace_full_owner(&paths, &desired)?;
                        Ok(true)
                    })?;
                    if claimed {
                        local_claim.retain();
                        return Ok(Self {
                            paths,
                            owner: desired,
                            active: true,
                            release_on_drop: true,
                        });
                    }
                }
                Some(owner) if owner.repo_identity_sha256 != paths.repo_identity_sha256 => {
                    bail!("workspace full permit owner repo identity 漂移");
                }
                Some(owner)
                    if owner.pid == desired.pid
                        && owner.birth_identity == desired.birth_identity =>
                {
                    bail!(
                        "workspace full permit owner 属于当前进程但没有匹配的内部 handle；拒绝 env/字符串重入"
                    );
                }
                Some(owner) if !workspace_full_owner_reclaimable(&owner)? => {
                    std::thread::sleep(WORKSPACE_FULL_WAIT_INTERVAL);
                }
                Some(stale) => {
                    let claimed = with_workspace_full_state_lock(&paths, || {
                        let Some(observed) = read_workspace_full_owner(&paths.owner)? else {
                            return Ok(false);
                        };
                        if observed != stale || !workspace_full_owner_reclaimable(&observed)? {
                            return Ok(false);
                        }
                        write_workspace_full_owner(&paths, &desired)?;
                        Ok(true)
                    })?;
                    if claimed {
                        local_claim.retain();
                        return Ok(Self {
                            paths,
                            owner: desired,
                            active: true,
                            release_on_drop: true,
                        });
                    }
                }
            }
        }
    }

    fn execution_guard(&self) -> Result<WorkspaceFullExecutionGuard> {
        if !self.active {
            bail!("workspace full permit 已释放");
        }
        WorkspaceFullExecutionGuard::begin(&self.paths, &self.owner)
    }

    /// Derive an exclusive same-process nested capability which cannot outlive this holder.
    ///
    /// The nested runner must present the same repository, gateRunId, and action. The mutable borrow
    /// prevents two handles from authorizing concurrent children under one owner record.
    pub fn reentry_handle(&mut self) -> Result<WorkspaceFullPermitHandle<'_>> {
        if !self.active {
            bail!("workspace full permit 已释放");
        }
        ensure_workspace_full_current_process(&self.owner)?;
        let owners = workspace_full_local_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.get(&self.paths.scope) != Some(&self.owner.token) {
            bail!("workspace full permit 的 process-local owner token 漂移");
        }
        Ok(WorkspaceFullPermitHandle { permit: self })
    }

    /// Release the capability immediately instead of waiting for scope exit.
    ///
    /// A failed durable transition leaves this holder active so the caller can repair a transient
    /// filesystem/identity problem and retry; process-local authority is cleared only after the
    /// owner record has been removed and the repository directory synced successfully.
    pub fn release(&mut self) -> Result<()> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        ensure_workspace_full_current_process(&self.owner)?;
        let mut owner_removed = false;
        let release_result = with_workspace_full_state_lock(&self.paths, || {
            let observed = read_workspace_full_owner(&self.paths.owner)?
                .context("workspace full permit release 缺 owner")?;
            if !same_workspace_full_authority(&observed, &self.owner) {
                bail!("workspace full permit release owner identity 漂移");
            }
            if !matches!(&observed.child, WorkspaceFullChildStateV1::Unspawned)
                && !workspace_full_child_converged(&observed.child)?
            {
                bail!("workspace full permit release 时 child/fixture registry 尚未收敛");
            }
            fs::remove_file(&self.paths.owner).with_context(|| {
                format!(
                    "删除 workspace full permit owner 失败: {}",
                    self.paths.owner.display()
                )
            })?;
            owner_removed = true;
            File::open(&self.paths.directory)?.sync_all()?;
            Ok(())
        });
        if owner_removed {
            remove_workspace_full_local_owner(&self.paths.scope, &self.owner.token);
            self.active = false;
        }
        release_result
    }
}

impl Drop for WorkspaceFullPermit {
    fn drop(&mut self) {
        if self.release_on_drop {
            if let Err(error) = self.release_inner() {
                eprintln!("workspace full permit release failed: {error:#}");
            }
        } else if self.active {
            eprintln!(
                "workspace full permit release retry suppressed after a propagated failure; owner remains fail-closed"
            );
        }
    }
}

/// Borrowed same-process authorization for nested full-gate runners.
///
/// Its field is private, exclusively borrowed, and tied to the outer holder, so an environment
/// variable or copied owner string cannot synthesize authority or move it to another repository.
#[derive(Debug)]
pub struct WorkspaceFullPermitHandle<'a> {
    permit: &'a mut WorkspaceFullPermit,
}

impl WorkspaceFullPermitHandle<'_> {
    fn validate(&self, root: &Path, gate_run_id: &str, action: &str) -> Result<()> {
        if !self.permit.active {
            bail!("workspace full permit reentry handle 已失效");
        }
        ensure_workspace_full_current_process(&self.permit.owner)?;
        let paths = workspace_full_paths(root)?;
        if paths.scope != self.permit.paths.scope
            || paths.repo_identity_sha256 != self.permit.owner.repo_identity_sha256
        {
            bail!("workspace full permit reentry handle repo identity 不匹配");
        }
        if self.permit.owner.gate_run_id != gate_run_id || self.permit.owner.action != action {
            bail!("workspace full permit reentry handle gateRunId/action 不匹配");
        }
        let owners = workspace_full_local_owners()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if owners.get(&self.permit.paths.scope) != Some(&self.permit.owner.token) {
            bail!("workspace full permit reentry handle token 不匹配");
        }
        Ok(())
    }

    fn execution_guard(
        &self,
        root: &Path,
        gate_run_id: &str,
        action: &str,
    ) -> Result<WorkspaceFullExecutionGuard> {
        self.validate(root, gate_run_id, action)?;
        self.permit.execution_guard()
    }
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

fn workspace_full_gate_run_id(tag: &str) -> Result<&str> {
    tag.rsplit('-')
        .next()
        .filter(|value| !value.trim().is_empty())
        .context("full gate tag 缺 gateRunId")
}

fn workspace_full_gate_action(
    identity: crate::ledger::GateAuditIdentity<'_>,
    name: &str,
) -> String {
    format!(
        "gate:{}:{}:{name}",
        identity.task_id(),
        identity.attempt_id().unwrap_or("pre-attempt")
    )
}

fn finish_workspace_full_gate<T>(
    mut permit: WorkspaceFullPermit,
    execution: WorkspaceFullExecutionGuard,
    gate_result: Result<T>,
) -> Result<T> {
    drop(execution);
    let release_result = permit.release();
    if release_result.is_err() {
        // The caller receives the first durable release failure. Do not let Drop silently retry
        // and change owner state after the returned result has already been chosen.
        permit.release_on_drop = false;
    }
    match (gate_result, release_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(gate_error), Ok(())) => Err(gate_error),
        (Ok(_), Err(release_error)) => Err(release_error)
            .context("workspace full permit release failed after gate completion"),
        (Err(gate_error), Err(release_error)) => Err(anyhow::anyhow!(
            "gate execution failed: {gate_error:#}; workspace full permit release also failed: {release_error:#}"
        )),
    }
}

/// Fresh production gate boundary.  Its closed audit identity is acquired and
/// held with the storage permit until the child and its process group have
/// been reaped. Full-gate serialization uses a separate repository permit and
/// never extends the ledger critical section across the child runtime.
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
    let gate_run_id = workspace_full_gate_run_id(tag)?;
    let action = workspace_full_gate_action(identity, name);
    let full_permit = WorkspaceFullPermit::acquire(root, gate_run_id, &action)?;
    let full_execution = full_permit.execution_guard()?;
    let gate_result = run_gate(name, spec, workdir, log_dir, tag);
    finish_workspace_full_gate(full_permit, full_execution, gate_result)
}

/// Full production gate under a storage permit held by a wider workflow. Callers must freshly
/// refresh that storage permit with the same closed identity immediately before invoking this
/// runner. The repository-wide full permit is acquired here and released after the child is reaped.
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
    let gate_run_id = workspace_full_gate_run_id(tag)?;
    let action = workspace_full_gate_action(identity, name);
    let full_permit = WorkspaceFullPermit::acquire(workdir, gate_run_id, &action)?;
    let full_execution = full_permit.execution_guard()?;
    let gate_result = run_gate(name, spec, workdir, log_dir, tag);
    finish_workspace_full_gate(full_permit, full_execution, gate_result)
}

/// Candidate-lane production gate under an already refreshed storage permit.
///
/// Unlike [`run_gate_with_permit_and_identity`], this narrow runner deliberately does not acquire
/// [`WorkspaceFullPermit`], so independent closed candidate lanes may execute concurrently.
#[allow(clippy::too_many_arguments)]
pub fn run_candidate_gate_with_permit_and_identity(
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

/// Execute a nested full gate using capabilities derived by the outer storage and workspace
/// holders. The borrowed full handle is the only supported same-process reentry path.
#[allow(clippy::too_many_arguments)]
pub fn run_full_gate_with_permits_and_identity(
    full_permit: &mut WorkspaceFullPermitHandle<'_>,
    _storage_permit: &crate::storage::StoragePermit,
    identity: crate::ledger::GateAuditIdentity<'_>,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    identity.validate()?;
    let gate_run_id = workspace_full_gate_run_id(tag)?;
    let action = workspace_full_gate_action(identity, name);
    let full_execution = full_permit.execution_guard(workdir, gate_run_id, &action)?;
    let gate_result = run_gate(name, spec, workdir, log_dir, tag);
    drop(full_execution);
    gate_result
}

/// Trial gate under a permit held by a wider workflow. Callers must freshly recheck that permit
/// with the same closed identity immediately before invoking this typed runner. Trial is a full
/// lane and therefore acquires the repository-wide permit before spawning.
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
    let gate_run_id = workspace_full_gate_run_id(tag)?;
    let action = workspace_full_gate_action(identity, name);
    let full_permit = WorkspaceFullPermit::acquire(workdir, gate_run_id, &action)?;
    let full_execution = full_permit.execution_guard()?;
    let gate_result = run_trial_gate(name, spec, workdir, log_dir, tag, target);
    finish_workspace_full_gate(full_permit, full_execution, gate_result)
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

#[cfg(unix)]
fn reset_gate_child_signal_dispositions(command: &mut Command) {
    // SAFETY: `signal(2)` is called only in the post-fork/pre-exec child for three fixed POSIX
    // signals. The closure performs no allocation or locking and reports errno directly to spawn.
    unsafe {
        command.pre_exec(|| {
            for signal in GATE_CHILD_DEFAULT_SIGNALS {
                if libc_signal(signal, SIG_DFL) == SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
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
    {
        cmd.process_group(0);
        reset_gate_child_signal_dispositions(&mut cmd);
    }
    let (stdout, stderr) = gate_log_sinks(&log_path)?;
    cmd.stdout(stdout).stderr(stderr);
    let full_preparation = prepare_workspace_full_child_if_owned(workdir, &registry_path)?;
    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            let spawn_error = anyhow::Error::new(error)
                .context(format!("门 {name} spawn 失败: {prog}"));
            return match &full_preparation {
                Some(preparation) => match cancel_workspace_full_child_preparation(preparation) {
                    Ok(()) => Err(spawn_error),
                    Err(cancel_error) => Err(anyhow::anyhow!(
                        "{spawn_error:#}; workspace full permit spawn cancellation also failed: {cancel_error:#}"
                    )),
                },
                None => Err(spawn_error),
            };
        }
    };
    let pgid = child.id();
    let full_child = match &full_preparation {
        Some(preparation) => {
            let lease = match capture_workspace_full_child(preparation, &mut child) {
                Ok(lease) => lease,
                Err(capture_error) => {
                    let cleanup = cleanup_unbound_workspace_full_child(preparation, &mut child);
                    return match cleanup {
                        Ok(()) => Err(capture_error).context(
                            "workspace full permit 无法捕获 exact child lease；unbound child 已安全收敛",
                        ),
                        Err(cleanup_error) => Err(anyhow::anyhow!(
                            "workspace full permit 无法捕获 exact child lease: {capture_error:#}; unbound cleanup also failed: {cleanup_error:#}"
                        )),
                    };
                }
            };
            match persist_workspace_full_child(preparation, &lease) {
                Ok(()) => Some(lease),
                Err(error) => {
                    return match recover_workspace_full_child_after_bind_failure(
                    preparation,
                    &lease,
                    &mut child,
                ) {
                    Ok(()) => Err(error).context(
                        "workspace full permit child bind 失败；exact child 已安全收敛",
                    ),
                    Err(recovery) => Err(anyhow::anyhow!(
                        "workspace full permit child bind 失败: {error:#}; exact recovery 亦失败: {recovery:#}"
                    )),
                };
                }
            }
        }
        None => None,
    };
    let heartbeat_path = gate_heartbeat_sidecar_path(log_dir, tag, name);
    let heartbeat =
        spawn_gate_heartbeat(&heartbeat_path, pgid, configured_gate_heartbeat_interval());
    let wait_result = child.wait_timeout(Duration::from_secs(timeout_seconds));
    let (status, wait_error, timed_out) = match wait_result {
        Ok(Some(status)) => (Some(status), None, false),
        Ok(None) => (None, None, true),
        Err(error) => (None, Some(error), false),
    };
    let cleanup_result = if status.is_some() {
        let primary_reap = match full_child.as_ref() {
            Some(binding) => reap_bound_workspace_full_child(binding),
            None => reap_gate_process_group(pgid),
        };
        let registry_reap = reap_gate_fixture_registry(&registry_path).map(|_| ());
        finish_gate_child_cleanup_after_reaps(
            full_child.as_ref(),
            &mut child,
            primary_reap,
            registry_reap,
        )
    } else {
        let primary_reap = match full_child.as_ref() {
            Some(binding) => reap_bound_workspace_full_child(binding),
            None => reap_gate_process_group(pgid),
        };
        let registry_reap = reap_gate_fixture_registry(&registry_path).map(|_| ());
        finish_gate_child_cleanup_after_reaps(
            full_child.as_ref(),
            &mut child,
            primary_reap,
            registry_reap,
        )
    };
    heartbeat.stop();
    let status = match (status, wait_error, timed_out, cleanup_result) {
        (Some(status), None, false, Ok(())) => status,
        (Some(_), None, false, Err(cleanup_error)) => {
            return Err(cleanup_error).context(format!(
                "门 {name} completed but process/fixture cleanup failed"
            ))
        }
        (None, None, true, Ok(())) => bail!("门 {name} 超时（>{timeout_seconds}s）"),
        (None, None, true, Err(cleanup_error)) => {
            bail!(
                "门 {name} 超时（>{timeout_seconds}s）；process/fixture cleanup also failed: {cleanup_error:#}"
            )
        }
        (None, Some(wait_error), false, Ok(())) => {
            return Err(wait_error).context(format!(
                "门 {name} child wait observation failed after safe cleanup"
            ))
        }
        (None, Some(wait_error), false, Err(cleanup_error)) => {
            bail!(
                "门 {name} child wait observation failed: {wait_error}; process/fixture cleanup also failed: {cleanup_error:#}"
            )
        }
        _ => bail!("门 {name} wait/cleanup state 非 canonical"),
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
    let gate_run_id = workspace_full_gate_run_id(tag)?;
    let action = format!("gate:{}:{name}", task.unwrap_or("pre-attempt"));
    let full_permit = WorkspaceFullPermit::acquire(root, gate_run_id, &action)?;
    let full_execution = full_permit.execution_guard()?;
    let gate_result = run_gate(name, spec, workdir, log_dir, tag);
    finish_workspace_full_gate(full_permit, full_execution, gate_result)
}

/// Execute a full gate under storage admission acquired by a wider atomic workflow.
///
/// Collect can keep storage coverage across seed replay and trial-worktree creation, while this
/// boundary independently acquires the repository full permit immediately around the child.
/// Candidate callers must use the explicitly named candidate wrapper instead.
pub fn run_gate_with_permit(
    _permit: &crate::storage::StoragePermit,
    name: &str,
    spec: &CommandSpec,
    workdir: &Path,
    log_dir: &Path,
    tag: &str,
) -> Result<GateResult> {
    let gate_run_id = workspace_full_gate_run_id(tag)?;
    let action = format!("gate:legacy:{name}");
    let full_permit = WorkspaceFullPermit::acquire(workdir, gate_run_id, &action)?;
    let full_execution = full_permit.execution_guard()?;
    let gate_result = run_gate(name, spec, workdir, log_dir, tag);
    finish_workspace_full_gate(full_permit, full_execution, gate_result)
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

    #[test]
    fn workspace_full_epoch_keeps_authenticated_birth_after_leader_exit() {
        let epoch = classify_workspace_full_child_sample(
            Some("owner-birth".to_string()),
            Some("owner-birth".to_string()),
            true,
            false,
        )
        .unwrap()
        .expect("authenticated birth must classify the exited-leader sample");
        assert!(matches!(
            epoch,
            WorkspaceFullChildEpoch::Anchored { birth_identity }
                if birth_identity == "owner-birth"
        ));
    }

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

    struct PerGroupCleanupControl {
        observations: BTreeMap<u32, std::collections::VecDeque<GateGroupObservation>>,
        signaled_pgids: Vec<u32>,
        failing_pgid: u32,
        elapsed: Duration,
    }

    impl GateReapControl for PerGroupCleanupControl {
        fn observe_group(
            &mut self,
            pgid: u32,
        ) -> std::result::Result<GateGroupObservation, String> {
            self.observations
                .get_mut(&pgid)
                .and_then(std::collections::VecDeque::pop_front)
                .ok_or_else(|| format!("missing observation for {pgid}"))
        }

        fn signal_kill(&mut self, pgid: u32) -> std::io::Result<()> {
            self.signaled_pgids.push(pgid);
            if pgid == self.failing_pgid {
                Err(std::io::Error::from_raw_os_error(5))
            } else {
                Ok(())
            }
        }

        fn elapsed(&self) -> Duration {
            self.elapsed
        }

        fn sleep(&mut self, duration: Duration) {
            self.elapsed += duration;
        }
    }

    impl InjectedKillControl {
        fn with_errno(
            observations: impl IntoIterator<Item = GateGroupObservation>,
            signal_errno: i32,
        ) -> Self {
            Self {
                observations: observations.into_iter().collect(),
                signal_errno,
                elapsed: Duration::ZERO,
                signaled_pgids: Vec::new(),
            }
        }

        fn eperm(observations: impl IntoIterator<Item = GateGroupObservation>) -> Self {
            Self::with_errno(observations, 1)
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
    fn b271_stale_registry_entry_is_reported_without_aborting_a_later_reap() {
        let dir = crate::util::test_scratch_dir("b271-stale-then-valid-control");
        let registry = dir.join("fixtures");
        let stale_pgid = 42_271;
        let valid_pgid = 42_272;
        fs::write(
            &registry,
            format!(
                "{stale_pgid}\t{stale_pgid}\tstale-birth\n\
                 {valid_pgid}\t{valid_pgid}\tvalid-birth\n"
            ),
        )
        .unwrap();
        let mut control = InjectedKillControl::with_errno(
            [
                GateGroupObservation::Members(vec![GateGroupMember::live(
                    stale_pgid,
                    "replacement-birth",
                )]),
                GateGroupObservation::Members(vec![GateGroupMember::live(
                    valid_pgid,
                    "valid-birth",
                )]),
                GateGroupObservation::Gone,
            ],
            3,
        );

        let report = reap_gate_fixture_registry_with_control(&registry, &mut control)
            .expect("a stale entry must not abandon a later valid entry");
        fs::remove_dir_all(dir).unwrap();

        assert_eq!(report.processed, vec![stale_pgid, valid_pgid]);
        assert_eq!(report.skipped_birth_mismatch, vec![stale_pgid]);
        assert_eq!(report.skipped_malformed, 0);
        println!(
            "B271_REAP_REPORT processed={:?} skipped_birth_mismatch={:?} skipped_malformed={}",
            report.processed, report.skipped_birth_mismatch, report.skipped_malformed
        );
        assert_eq!(
            control.signaled_pgids,
            vec![valid_pgid],
            "the stale process group must not be signalled"
        );
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
        let report = reap_gate_fixture_registry_reporting(&registry)
            .expect("a stale registry anchor must be reported and skipped");
        assert_eq!(report.processed, vec![pgid]);
        assert_eq!(report.skipped_birth_mismatch, vec![pgid]);
        assert_eq!(report.skipped_malformed, 0);
        assert!(
            process_group_exists(pgid),
            "positive birth-identity mismatch must refuse the signal"
        );
        reap_gate_process_group(pgid).unwrap();
        let _ = fixture.wait();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn registry_cleanup_visits_later_groups_after_an_earlier_signal_error() {
        let dir = crate::util::test_scratch_dir("b306-registry-error-continuation");
        let registry = dir.join("fixtures");
        let first = 91_361u32;
        let second = 91_362u32;
        fs::write(
            &registry,
            format!("{first}\t{first}\tbirth-first\n{second}\t{second}\tbirth-second\n"),
        )
        .unwrap();
        let mut control = PerGroupCleanupControl {
            observations: BTreeMap::from([
                (
                    first,
                    std::collections::VecDeque::from([
                        GateGroupObservation::Members(vec![GateGroupMember::live(
                            first,
                            "birth-first",
                        )]),
                    ]),
                ),
                (
                    second,
                    std::collections::VecDeque::from([
                        GateGroupObservation::Members(vec![GateGroupMember::live(
                            second,
                            "birth-second",
                        )]),
                        GateGroupObservation::Gone,
                    ]),
                ),
            ]),
            signaled_pgids: Vec::new(),
            failing_pgid: first,
            elapsed: Duration::ZERO,
        };
        let error = reap_gate_fixture_registry_with_control(&registry, &mut control)
            .expect_err("the first non-tolerated signal error remains visible");
        assert!(format!("{error:#}").contains(&first.to_string()));
        assert_eq!(control.signaled_pgids, vec![first, second]);
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
