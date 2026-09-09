//! Fail-closed storage admission for operations which can materially grow the repository.
//!
//! The guard deliberately uses absolute byte floors.  A successful check returns a
//! [`StoragePermit`] which reserves the operation estimate under one process-wide lock; keeping
//! the permit alive until the guarded operation completes closes the check/use race.  Reclaim and
//! round-close operations are named exemptions so a full volume can still rescue itself.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};

use crate::{binding, ledger};
use ledger::GateAuditIdentity;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const DEFAULT_FLOOR_BYTES: u64 = 8 * GIB;
const DEFAULT_AUDIT_RESERVE_BYTES: u64 = 16 * MIB;
const DEFAULT_DISPATCH_ESTIMATE_BYTES: u64 = 2 * GIB;
const DEFAULT_WAKE_ESTIMATE_BYTES: u64 = 4 * GIB;
const DEFAULT_GATE_ESTIMATE_BYTES: u64 = 38 * GIB;
const AUDIT_RESERVE_REL: &str = "coordination/runtime/.storage-audit-reserve";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardEntry {
    Dispatch,
    WakeReview,
    Gate,
    /// Local diagnostic capacity, never usable as a production gate audit permit.
    Diagnostic,
    /// Opening a round reserves no build estimate and still enforces the existing floor.
    RoundOpen,
    Reclaim,
    RoundClose,
}

impl GuardEntry {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch",
            Self::WakeReview => "wake-review",
            Self::Gate => "gate",
            Self::Diagnostic => "diagnostic",
            Self::RoundOpen => "round-open",
            Self::Reclaim => "reclaim",
            Self::RoundClose => "round-close",
        }
    }

    pub fn is_reclaim(self) -> bool {
        matches!(self, Self::Reclaim | Self::RoundClose)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageThreshold {
    floor_bytes: u64,
}

impl StorageThreshold {
    pub fn from_floor_bytes(floor_bytes: u64) -> Self {
        Self { floor_bytes }
    }

    pub fn floor_bytes(self) -> u64 {
        self.floor_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    Ok {
        available_bytes: u64,
        /// Kept for audit output only.  It never participates in admission.
        total_bytes: u64,
    },
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOutcome {
    Allow {
        available_bytes: Option<u64>,
        threshold_bytes: u64,
    },
    Refuse {
        available_bytes: u64,
        threshold_bytes: u64,
        entry: GuardEntry,
        probe: &'static str,
    },
}

/// Pure absolute-byte decision.  `total_bytes` is intentionally ignored.
pub fn storage_guard_decision(
    threshold: &StorageThreshold,
    entry: GuardEntry,
    probe: &ProbeResult,
) -> GuardOutcome {
    if entry.is_reclaim() {
        return GuardOutcome::Allow {
            available_bytes: match probe {
                ProbeResult::Ok {
                    available_bytes, ..
                } => Some(*available_bytes),
                ProbeResult::Failed { .. } => None,
            },
            threshold_bytes: threshold.floor_bytes,
        };
    }
    match probe {
        ProbeResult::Ok {
            available_bytes, ..
        } if *available_bytes >= threshold.floor_bytes => GuardOutcome::Allow {
            available_bytes: Some(*available_bytes),
            threshold_bytes: threshold.floor_bytes,
        },
        ProbeResult::Ok {
            available_bytes, ..
        } => GuardOutcome::Refuse {
            available_bytes: *available_bytes,
            threshold_bytes: threshold.floor_bytes,
            entry,
            probe: "low",
        },
        ProbeResult::Failed { .. } => GuardOutcome::Refuse {
            available_bytes: 0,
            threshold_bytes: threshold.floor_bytes,
            entry,
            probe: "failed",
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReservationKey {
    fsid: u64,
    // Production reservations are process-wide per physical filesystem. Unit tests create many
    // independent repositories in one libtest process, so only that harness adds a repo domain.
    #[cfg(test)]
    test_root: Option<PathBuf>,
}

impl ReservationKey {
    fn new(_root: Option<&Path>, fsid: u64) -> Self {
        Self {
            fsid,
            #[cfg(test)]
            test_root: _root.map(Path::to_path_buf),
        }
    }
}

static OUTSTANDING: OnceLock<Mutex<HashMap<ReservationKey, u64>>> = OnceLock::new();

fn reservations() -> &'static Mutex<HashMap<ReservationKey, u64>> {
    OUTSTANDING.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug)]
struct TestScratchGateLane;

fn test_scratch_gate_lane_state() -> &'static (Mutex<bool>, Condvar) {
    static STATE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
    STATE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

fn is_direct_test_scratch_repository(root: &Path) -> bool {
    let Ok(root) = fs::canonicalize(root) else {
        return false;
    };
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(orch_root) = manifest.parent().and_then(Path::parent) else {
        return false;
    };
    let Ok(test_scratch) = fs::canonicalize(orch_root.join("target/test-tmp")) else {
        return false;
    };
    root.parent() == Some(test_scratch.as_path())
}

impl TestScratchGateLane {
    fn acquire(root: &Path, entry: GuardEntry) -> Option<Self> {
        if !matches!(entry, GuardEntry::Gate | GuardEntry::Diagnostic)
            || !is_direct_test_scratch_repository(root)
        {
            return None;
        }
        let (active, changed) = test_scratch_gate_lane_state();
        let mut active = active.lock().unwrap_or_else(|error| error.into_inner());
        while *active {
            active = changed
                .wait(active)
                .unwrap_or_else(|error| error.into_inner());
        }
        *active = true;
        Some(Self)
    }
}

impl Drop for TestScratchGateLane {
    fn drop(&mut self) {
        let (active, changed) = test_scratch_gate_lane_state();
        let mut active = active.lock().unwrap_or_else(|error| error.into_inner());
        *active = false;
        changed.notify_one();
    }
}

/// Admission token.  Dropping it returns every per-filesystem reservation.
#[derive(Debug)]
pub struct StoragePermit {
    reservations: Vec<(ReservationKey, u64)>,
    entry: GuardEntry,
    test_scratch_gate_lane: Option<TestScratchGateLane>,
}

impl StoragePermit {
    /// Single-probe constructor retained as the small, deterministic contract surface.
    pub fn acquire(
        threshold: &StorageThreshold,
        entry: GuardEntry,
        probe: &ProbeResult,
        operation_estimate_bytes: u64,
    ) -> std::result::Result<Self, GuardOutcome> {
        if entry.is_reclaim() {
            return Ok(Self {
                reservations: Vec::new(),
                entry,
                test_scratch_gate_lane: None,
            });
        }
        let ProbeResult::Ok {
            available_bytes,
            total_bytes,
        } = probe
        else {
            return Err(storage_guard_decision(threshold, entry, probe));
        };
        // The synthetic key is stable for equal fixture probes.  Production uses the actual
        // filesystem id through `acquire_filesystems` below.
        let synthetic_fsid = total_bytes.rotate_left(17) ^ available_bytes.rotate_right(7);
        Self::acquire_filesystems(
            threshold,
            entry,
            None,
            &[(synthetic_fsid, probe.clone())],
            operation_estimate_bytes,
        )
    }

    fn acquire_filesystems(
        threshold: &StorageThreshold,
        entry: GuardEntry,
        root: Option<&Path>,
        probes: &[(u64, ProbeResult)],
        operation_estimate_bytes: u64,
    ) -> std::result::Result<Self, GuardOutcome> {
        if entry.is_reclaim() {
            return Ok(Self {
                reservations: Vec::new(),
                entry,
                test_scratch_gate_lane: None,
            });
        }
        let mut outstanding = reservations()
            .lock()
            .expect("storage reservation lock poisoned");
        for (fsid, probe) in probes {
            let ProbeResult::Ok {
                available_bytes, ..
            } = probe
            else {
                return Err(GuardOutcome::Refuse {
                    available_bytes: 0,
                    threshold_bytes: threshold
                        .floor_bytes
                        .saturating_add(operation_estimate_bytes),
                    entry,
                    probe: "failed",
                });
            };
            let key = ReservationKey::new(root, *fsid);
            let reserved = outstanding.get(&key).copied().unwrap_or(0);
            let required = threshold
                .floor_bytes
                .saturating_add(operation_estimate_bytes);
            if available_bytes.saturating_sub(reserved) < required {
                return Err(GuardOutcome::Refuse {
                    available_bytes: available_bytes.saturating_sub(reserved),
                    threshold_bytes: required,
                    entry,
                    probe: "low",
                });
            }
        }
        let mut held = Vec::with_capacity(probes.len());
        for (fsid, _) in probes {
            let key = ReservationKey::new(root, *fsid);
            *outstanding.entry(key.clone()).or_default() = outstanding
                .get(&key)
                .copied()
                .unwrap_or(0)
                .saturating_add(operation_estimate_bytes);
            held.push((key, operation_estimate_bytes));
        }
        Ok(Self {
            reservations: held,
            entry,
            test_scratch_gate_lane: None,
        })
    }

    /// Re-probe a gate already covered by this permit without reserving its estimate twice.
    /// Every probed filesystem must be part of the original permit; discovering a new volume
    /// after the outer admission is a fail-closed refusal.
    fn refresh_filesystems(
        &self,
        threshold: &StorageThreshold,
        entry: GuardEntry,
        root: Option<&Path>,
        probes: &[(u64, ProbeResult)],
        operation_estimate_bytes: u64,
    ) -> std::result::Result<(), GuardOutcome> {
        if entry != GuardEntry::Gate || self.entry != GuardEntry::Gate {
            return Err(GuardOutcome::Refuse {
                available_bytes: 0,
                threshold_bytes: threshold
                    .floor_bytes
                    .saturating_add(operation_estimate_bytes),
                entry,
                probe: "uncovered",
            });
        }
        let outstanding = reservations()
            .lock()
            .expect("storage reservation lock poisoned");
        for (fsid, probe) in probes {
            let required = threshold
                .floor_bytes
                .saturating_add(operation_estimate_bytes);
            let ProbeResult::Ok {
                available_bytes, ..
            } = probe
            else {
                return Err(GuardOutcome::Refuse {
                    available_bytes: 0,
                    threshold_bytes: required,
                    entry,
                    probe: "failed",
                });
            };
            let key = ReservationKey::new(root, *fsid);
            let Some(own_reservation) = self
                .reservations
                .iter()
                .find_map(|(held_key, bytes)| (held_key == &key).then_some(*bytes))
            else {
                return Err(GuardOutcome::Refuse {
                    available_bytes: *available_bytes,
                    threshold_bytes: required,
                    entry,
                    probe: "uncovered",
                });
            };
            let Some(total_reservations) = outstanding.get(&key).copied() else {
                return Err(GuardOutcome::Refuse {
                    available_bytes: *available_bytes,
                    threshold_bytes: required,
                    entry,
                    probe: "uncovered",
                });
            };
            let Some(other_reservations) = total_reservations.checked_sub(own_reservation) else {
                return Err(GuardOutcome::Refuse {
                    available_bytes: *available_bytes,
                    threshold_bytes: required,
                    entry,
                    probe: "uncovered",
                });
            };
            let effective_available = available_bytes.saturating_sub(other_reservations);
            if effective_available < required {
                return Err(GuardOutcome::Refuse {
                    available_bytes: effective_available,
                    threshold_bytes: required,
                    entry,
                    probe: "low",
                });
            }
        }
        Ok(())
    }
}

impl Drop for StoragePermit {
    fn drop(&mut self) {
        let Ok(mut outstanding) = reservations().lock() else {
            return;
        };
        for (key, bytes) in self.reservations.drain(..) {
            if let Some(value) = outstanding.get_mut(&key) {
                *value = value.saturating_sub(bytes);
                if *value == 0 {
                    outstanding.remove(&key);
                }
            }
        }
        self.test_scratch_gate_lane.take();
    }
}

#[cfg(test)]
pub(crate) fn fixture_gate_permit(root: &Path, paths: &[PathBuf]) -> StoragePermit {
    // A one-byte reservation keeps the fixture honest (refresh can verify the permit is
    // registered) without making parallel unit tests consume production-sized headroom.
    let held = paths
        .iter()
        .filter_map(|path| nearest_existing(path))
        .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.dev()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|fsid| (ReservationKey::new(Some(root), fsid), 1))
        .collect::<Vec<_>>();
    {
        let mut outstanding = reservations()
            .lock()
            .expect("storage reservation lock poisoned");
        for (key, bytes) in &held {
            *outstanding.entry(key.clone()).or_default() = outstanding
                .get(key)
                .copied()
                .unwrap_or(0)
                .saturating_add(*bytes);
        }
    }
    StoragePermit {
        reservations: held,
        entry: GuardEntry::Gate,
        test_scratch_gate_lane: None,
    }
}

#[derive(Debug, Clone, Copy)]
struct StorageSettings {
    threshold: StorageThreshold,
    audit_reserve_bytes: u64,
    dispatch_estimate_bytes: u64,
    wake_estimate_bytes: u64,
    gate_estimate_bytes: u64,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            threshold: StorageThreshold::from_floor_bytes(DEFAULT_FLOOR_BYTES),
            audit_reserve_bytes: DEFAULT_AUDIT_RESERVE_BYTES,
            dispatch_estimate_bytes: DEFAULT_DISPATCH_ESTIMATE_BYTES,
            wake_estimate_bytes: DEFAULT_WAKE_ESTIMATE_BYTES,
            gate_estimate_bytes: DEFAULT_GATE_ESTIMATE_BYTES,
        }
    }
}

impl StorageSettings {
    fn for_root(root: &Path) -> Result<Self> {
        let mut settings = Self::default();
        let machine = match binding::load_machine_config(root)? {
            Some(config) => Some(config),
            None => binding::resolve_main_repo_machine_config(root)?,
        };
        if let Some(machine) = machine {
            // Machine overlay may only tighten the compiled safety defaults.
            settings.threshold.floor_bytes = settings
                .threshold
                .floor_bytes
                .max(machine.storage.floor_bytes.unwrap_or(0));
            settings.audit_reserve_bytes = settings
                .audit_reserve_bytes
                .max(machine.storage.audit_reserve_bytes.unwrap_or(0));
            settings.dispatch_estimate_bytes = settings
                .dispatch_estimate_bytes
                .max(machine.storage.dispatch_estimate_bytes.unwrap_or(0));
            settings.wake_estimate_bytes = settings
                .wake_estimate_bytes
                .max(machine.storage.wake_estimate_bytes.unwrap_or(0));
            settings.gate_estimate_bytes = settings
                .gate_estimate_bytes
                .max(machine.storage.gate_estimate_bytes.unwrap_or(0));
        }
        Ok(settings)
    }

    fn estimate(self, entry: GuardEntry) -> u64 {
        match entry {
            GuardEntry::Dispatch => self.dispatch_estimate_bytes,
            GuardEntry::WakeReview => self.wake_estimate_bytes,
            GuardEntry::Gate | GuardEntry::Diagnostic => self.gate_estimate_bytes,
            GuardEntry::Reclaim | GuardEntry::RoundClose | GuardEntry::RoundOpen => 0,
        }
    }
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cursor = path;
    loop {
        if cursor.exists() {
            return Some(cursor.to_path_buf());
        }
        cursor = cursor.parent()?;
    }
}

fn probe_path(path: &Path) -> ProbeResult {
    // `df` is the probe itself; no guarded git/provider/gate child has been spawned yet.  `-P`
    // gives one POSIX row and `-k` fixes the block unit, on both macOS and Linux.
    let output = match Command::new("df").args(["-Pk"]).arg(path).output() {
        Ok(output) => output,
        Err(error) => {
            return ProbeResult::Failed {
                reason: format!("spawn df: {error}"),
            }
        }
    };
    if !output.status.success() {
        return ProbeResult::Failed {
            reason: format!(
                "df exit {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(row) = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .next_back()
    else {
        return ProbeResult::Failed {
            reason: "df returned no data row".into(),
        };
    };
    let columns = row.split_whitespace().collect::<Vec<_>>();
    let parsed = columns
        .get(1)
        .zip(columns.get(3))
        .and_then(|(total, available)| {
            Some((total.parse::<u64>().ok()?, available.parse::<u64>().ok()?))
        });
    match parsed {
        Some((total_kib, available_kib)) => ProbeResult::Ok {
            available_bytes: available_kib.saturating_mul(1024),
            total_bytes: total_kib.saturating_mul(1024),
        },
        None => ProbeResult::Failed {
            reason: format!("unparseable df row: {row}"),
        },
    }
}

/// Deduplicate requested paths by filesystem id, then probe every distinct volume.
fn probe_filesystems(paths: &[PathBuf]) -> Vec<(u64, ProbeResult)> {
    #[cfg(test)]
    if let Some(probes) = TEST_FILESYSTEMS.with(|slot| {
        let borrowed = slot.borrow();
        let (root, entries) = borrowed.as_ref()?;
        if !paths.iter().all(|path| path.starts_with(root)) {
            return None;
        }
        let mut selected = BTreeMap::new();
        for path in paths {
            let (_, (id, probe)) = entries
                .iter()
                .filter(|(prefix, _)| path.starts_with(prefix))
                .max_by_key(|(prefix, _)| prefix.components().count())?;
            selected.insert(*id, probe.clone());
        }
        Some(selected.into_iter().collect())
    }) {
        return probes;
    }
    let mut filesystem_paths = BTreeMap::<u64, PathBuf>::new();
    let mut failures = Vec::new();
    for path in paths {
        let Some(existing) = nearest_existing(path) else {
            failures.push((
                u64::MAX.saturating_sub(failures.len() as u64),
                ProbeResult::Failed {
                    reason: format!("no existing ancestor for {}", path.display()),
                },
            ));
            continue;
        };
        match fs::metadata(&existing) {
            Ok(metadata) => {
                // `dev` is the stable filesystem identity exposed by the host OS (`fsid`).
                filesystem_paths.entry(metadata.dev()).or_insert(existing);
            }
            Err(error) => failures.push((
                u64::MAX.saturating_sub(failures.len() as u64),
                ProbeResult::Failed {
                    reason: format!("stat {}: {error}", existing.display()),
                },
            )),
        }
    }
    failures.extend(
        filesystem_paths
            .into_iter()
            .map(|(filesystem, path)| (filesystem, probe_path(&path))),
    );
    failures
}

/// One read-only filesystem observation. An unavailable value is never encoded as zero.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemSpace {
    /// Host filesystem device identity; observations are deduplicated by this value.
    pub device: u64,
    /// Available bytes measured by the operating system, or null on failure.
    pub available_bytes: Option<u64>,
    /// Total filesystem bytes, for reporting only and never for admission.
    pub total_bytes: Option<u64>,
    /// The actual probe failure, if an observation could not be obtained.
    pub error: Option<String>,
}

/// Observe the distinct filesystems containing the supplied paths without allocating or writing.
/// Nonexistent paths use their nearest existing ancestor; callers must supply actual cache paths.
pub fn observe_filesystem_space(paths: &[PathBuf]) -> Vec<FilesystemSpace> {
    probe_filesystems(paths)
        .into_iter()
        .map(|(device, probe)| match probe {
            ProbeResult::Ok {
                available_bytes,
                total_bytes,
            } => FilesystemSpace {
                device,
                available_bytes: Some(available_bytes),
                total_bytes: Some(total_bytes),
                error: None,
            },
            ProbeResult::Failed { reason } => FilesystemSpace {
                device,
                available_bytes: None,
                total_bytes: None,
                error: Some(reason),
            },
        })
        .collect()
}

/// Enforce the existing disk floor before new round files are created, with no task ledger writes.
/// Cleanup remains a separate exempt operation; this permit adds no dispatch/review/build estimate.
pub fn guard_round_open(root: &Path) -> Result<StoragePermit> {
    let settings = StorageSettings::for_root(root)?;
    let probes = probe_filesystems(&[root.join("coordination/rounds")]);
    if probes.is_empty() {
        bail!("round-open storage probe unavailable");
    }
    StoragePermit::acquire_filesystems(
        &settings.threshold,
        GuardEntry::RoundOpen,
        Some(root),
        &probes,
        settings.estimate(GuardEntry::RoundOpen),
    )
    .map_err(|error| anyhow::anyhow!("round-open storage admission refused: {error:?}"))
}

#[cfg(test)]
thread_local! {
    static TEST_FILESYSTEMS: std::cell::RefCell<Option<(PathBuf, BTreeMap<PathBuf, (u64, ProbeResult)>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_filesystems<T>(
    root: &Path,
    entries: BTreeMap<PathBuf, (u64, ProbeResult)>,
    action: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_FILESYSTEMS.with(|slot| *slot.borrow_mut() = None);
        }
    }
    TEST_FILESYSTEMS.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some((root.into(), entries));
    });
    let _reset = Reset;
    action()
}

fn event_attempt_id(event: &EventRecord) -> Option<&str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("attemptId"))
        .and_then(serde_json::Value::as_str)
}

/// Pure ledger fold: storage suppression starts after the selected attempt's latest start and
/// ends only at a later `state:"recovered"` storage fact.  It performs no filesystem IO.
pub fn storage_interruption_active(events: &[EventRecord], task: &str, attempt: &str) -> bool {
    #[derive(Clone, Copy)]
    enum InterruptionState {
        Clear,
        LegacyRefused,
        CanonicalRefused,
    }

    let start = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "AttemptStarted"
                && event.task_id.as_deref() == Some(task)
                && event_attempt_id(event) == Some(attempt)
        })
        .max_by(|(left_position, left), (right_position, right)| {
            left.ts
                .cmp(&right.ts)
                .then(left_position.cmp(right_position))
        })
        .map(|(position, event)| (position + 1, event.ts.as_str(), event.round.as_deref()));
    let (start_position, start_ts, start_round) = start.unwrap_or((0, "", None));
    let state = events[start_position..]
        .iter()
        .fold(InterruptionState::Clear, |state, event| {
            // Ledger timestamps are canonical RFC3339 UTC strings; lexical order is chronological.
            if event.ts.as_str() < start_ts {
                return state;
            }
            if event.kind != "EscalationRaised" || event.task_id.as_deref() != Some(task) {
                return state;
            }
            let Some(payload) = event.payload.as_ref() else {
                return state;
            };
            if payload.get("stage").and_then(serde_json::Value::as_str) != Some("storage") {
                return state;
            }
            if let Some(audit) = ledger::canonical_gate_storage_audit_event(event) {
                let expected = GateAuditIdentity::Attempt {
                    task_id: task,
                    attempt_id: attempt,
                };
                if audit.identity != expected || Some(audit.round) != start_round {
                    return state;
                }
                return if audit.recovered {
                    match state {
                        InterruptionState::CanonicalRefused => InterruptionState::Clear,
                        // An exact recovery is still unpaired when the only preceding refusal is
                        // legacy-shaped; it must not synthesize canonical authority retroactively.
                        other => other,
                    }
                } else {
                    InterruptionState::CanonicalRefused
                };
            }
            // Historical pre-B263 storage facts had no exact audit shape. Preserve their tolerant
            // liveness semantics in their own lane.  A malformed same-attempt recovery must never
            // clear a canonical refusal merely because it happens to carry `state=recovered`.
            if event_attempt_id(event).is_some_and(|candidate| candidate != attempt) {
                return state;
            }
            if payload.get("state").and_then(serde_json::Value::as_str) == Some("recovered") {
                return match state {
                    InterruptionState::LegacyRefused => InterruptionState::Clear,
                    other => other,
                };
            }
            match state {
                InterruptionState::CanonicalRefused => InterruptionState::CanonicalRefused,
                _ => InterruptionState::LegacyRefused,
            }
        });
    !matches!(state, InterruptionState::Clear)
}

fn storage_refusal_active(
    events: &[EventRecord],
    task: Option<&str>,
    attempt: Option<&str>,
) -> bool {
    events.iter().fold(false, |active, event| {
        if event.kind != "EscalationRaised" || event.task_id.as_deref() != task {
            return active;
        }
        let Some(payload) = event.payload.as_ref() else {
            return active;
        };
        if payload.get("stage").and_then(serde_json::Value::as_str) != Some("storage") {
            return active;
        }
        if event_attempt_id(event) != attempt {
            return active;
        }
        payload.get("state").and_then(serde_json::Value::as_str) != Some("recovered")
    })
}

fn ensure_audit_reserve(root: &Path, bytes: u64) -> Result<()> {
    let reserve = root.join(AUDIT_RESERVE_REL);
    if reserve
        .metadata()
        .is_ok_and(|metadata| metadata.len() >= bytes)
    {
        return Ok(());
    }
    let parent = reserve.parent().context("audit reserve missing parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".storage-audit-reserve-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("create audit reserve {}", temporary.display()))?;
    let zeros = vec![0u8; MIB as usize];
    let mut remaining = bytes;
    while remaining > 0 {
        let chunk = remaining.min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..chunk])?;
        remaining -= chunk as u64;
    }
    file.sync_all()?;
    fs::rename(&temporary, &reserve)?;
    Ok(())
}

fn append_storage_event(
    root: &Path,
    round: &str,
    task: Option<&str>,
    entry: GuardEntry,
    event: EventRecord,
) -> Result<()> {
    let append = |event| {
        if entry == GuardEntry::Gate {
            ledger::append_storage_audit(root, round, event)
        } else {
            ledger::append(root, round, &[event])
        }
    };
    match append(event.clone()) {
        Ok(()) => Ok(()),
        Err(first) => {
            // Release the non-sparse reserve and retry exactly once.  The reserve is replenished
            // by the next admitted operation; failure remains loud and fail-closed.
            let reserve = root.join(AUDIT_RESERVE_REL);
            if let Err(release_error) = fs::remove_file(&reserve) {
                eprintln!(
                    "storage guard: ledger append failed and audit reserve release failed: {release_error}"
                );
            }
            append(event).with_context(|| {
                format!(
                    "storage guard audit append failed after reserve release (task={}): {first:#}",
                    task.unwrap_or("-")
                )
            })
        }
    }
}

fn first_probe_failure(probes: &[(u64, ProbeResult)]) -> Option<&str> {
    probes.iter().find_map(|(_, result)| match result {
        ProbeResult::Failed { reason } => Some(reason.as_str()),
        ProbeResult::Ok { .. } => None,
    })
}

fn minimum_available_bytes(probes: &[(u64, ProbeResult)]) -> Option<u64> {
    probes
        .iter()
        .filter_map(|(_, result)| match result {
            ProbeResult::Ok {
                available_bytes, ..
            } => Some(*available_bytes),
            ProbeResult::Failed { .. } => None,
        })
        .min()
}

fn gibibytes(bytes: u64) -> String {
    if bytes % GIB == 0 {
        format!("{} GiB", bytes / GIB)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

fn storage_refusal_message(
    entry: GuardEntry,
    probe: &str,
    available_bytes: u64,
    threshold_bytes: u64,
    probe_reason: Option<&str>,
) -> String {
    let current = if probe == "failed" {
        "未知".to_string()
    } else {
        gibibytes(available_bytes)
    };
    format!(
        "storage guard refused {}: 需要 {} / 当前 {} / 先运行 `orch sites gc`; \
         probe={} availableBytes={} thresholdBytes={} probeReason={}",
        entry.as_str(),
        gibibytes(threshold_bytes),
        current,
        probe,
        available_bytes,
        threshold_bytes,
        probe_reason.unwrap_or("none")
    )
}

#[allow(clippy::too_many_arguments)]
fn storage_refusal_event(
    task: Option<&str>,
    round: &str,
    attempt: Option<&str>,
    entry: GuardEntry,
    probe: &str,
    available_bytes: u64,
    threshold_bytes: u64,
    probe_reason: Option<&str>,
) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "runtime:orch",
        task,
        Some(round),
        serde_json::json!({
            "stage": "storage",
            "availableBytes": available_bytes,
            "thresholdBytes": threshold_bytes,
            "entry": entry.as_str(),
            "probe": probe,
            "probeReason": probe_reason,
            "attemptId": attempt,
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn storage_recovery_event(
    task: Option<&str>,
    round: &str,
    attempt: Option<&str>,
    entry: GuardEntry,
    available_bytes: Option<u64>,
    threshold_bytes: u64,
    probe_reason: Option<&str>,
) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "runtime:orch",
        task,
        Some(round),
        serde_json::json!({
            "stage": "storage",
            "state": "recovered",
            "availableBytes": available_bytes,
            "thresholdBytes": threshold_bytes,
            "entry": entry.as_str(),
            "probe": "ok",
            "probeReason": probe_reason,
            "attemptId": attempt,
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn audit_admission<T>(
    root: &Path,
    round: &str,
    task: Option<&str>,
    attempt: Option<&str>,
    gate_identity: Option<GateAuditIdentity<'_>>,
    entry: GuardEntry,
    settings: StorageSettings,
    probes: &[(u64, ProbeResult)],
    threshold_bytes: u64,
    probe_reason: Option<&str>,
    decision: std::result::Result<T, GuardOutcome>,
) -> Result<T> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    // Gate decisions are folded atomically by `append_storage_audit` under ledger.lock.
    // Non-gate compatibility paths retain their historical tolerant fold.
    let refusal_active = match gate_identity {
        Some(_) => false,
        None => {
            let events = read_ledger(&ledger_path)
                .map(|read| read.events)
                .unwrap_or_default();
            storage_refusal_active(&events, task, attempt)
        }
    };
    match decision {
        Ok(value) => {
            ensure_audit_reserve(root, settings.audit_reserve_bytes)
                .context("storage guard could not provision non-sparse audit reserve")?;
            if gate_identity.is_some() || refusal_active {
                let recovery = storage_recovery_event(
                    task,
                    round,
                    attempt,
                    entry,
                    minimum_available_bytes(probes),
                    threshold_bytes,
                    probe_reason,
                );
                append_storage_event(root, round, task, entry, recovery)?;
            }
            Ok(value)
        }
        Err(outcome) => {
            let GuardOutcome::Refuse {
                available_bytes,
                threshold_bytes,
                probe,
                ..
            } = outcome
            else {
                unreachable!("permit rejection is always a refusal")
            };
            let refusal = storage_refusal_message(
                entry,
                probe,
                available_bytes,
                threshold_bytes,
                probe_reason,
            );
            // Consecutive same-identity refusals are one durable fact, not ledger noise.
            if gate_identity.is_some() || !refusal_active {
                let event = storage_refusal_event(
                    task,
                    round,
                    attempt,
                    entry,
                    probe,
                    available_bytes,
                    threshold_bytes,
                    probe_reason,
                );
                if let Err(audit_error) = append_storage_event(root, round, task, entry, event) {
                    bail!("{refusal}; durable storage audit append failed: {audit_error:#}");
                }
            }
            bail!("{refusal}")
        }
    }
}

/// Guard a growing operation.  The returned permit must remain in scope until the operation and
/// all of its child processes finish.
pub fn guard_operation(
    root: &Path,
    round: &str,
    task: Option<&str>,
    attempt: Option<&str>,
    entry: GuardEntry,
    paths: &[PathBuf],
) -> Result<StoragePermit> {
    if entry == GuardEntry::Gate {
        bail!("gate admission requires typed GateAuditIdentity");
    }
    if entry == GuardEntry::Diagnostic {
        bail!("diagnostic admission requires the diagnostic-only no-ledger API");
    }
    if entry == GuardEntry::RoundOpen {
        bail!("round-open admission requires the round-open no-ledger API");
    }
    guard_operation_inner(root, round, task, attempt, None, entry, paths)
}

/// Reserve the normal build budget for a local diagnostic, without inventing a task or round.
/// Uses the same tightened machine settings and real filesystem reservation domain as gates.
/// This permit has Diagnostic identity, cannot refresh a gate, and emits no ledger/audit event.
pub fn guard_diagnostic_operation(root: &Path, paths: &[PathBuf]) -> Result<StoragePermit> {
    let lane = TestScratchGateLane::acquire(root, GuardEntry::Diagnostic);
    let settings = StorageSettings::for_root(root)?;
    let probes = probe_filesystems(paths);
    if probes.is_empty() {
        bail!("diagnostic storage admission refused: no filesystem probe target");
    }
    let mut permit = StoragePermit::acquire_filesystems(
        &settings.threshold,
        GuardEntry::Diagnostic,
        Some(root),
        &probes,
        settings.estimate(GuardEntry::Diagnostic),
    )
    .map_err(|refusal| anyhow::anyhow!("diagnostic storage admission refused: {refusal:?}"))?;
    permit.test_scratch_gate_lane = lane;
    Ok(permit)
}

/// Typed gate-only admission.  The API cannot represent a known attempt as
/// null: pre-attempt and attempt-bound callers must choose distinct variants.
pub fn guard_gate_operation(
    root: &Path,
    round: &str,
    identity: GateAuditIdentity<'_>,
    paths: &[PathBuf],
) -> Result<StoragePermit> {
    identity.validate()?;
    guard_operation_inner(
        root,
        round,
        Some(identity.task_id()),
        identity.attempt_id(),
        Some(identity),
        GuardEntry::Gate,
        paths,
    )
}

#[allow(clippy::too_many_arguments)]
fn guard_operation_inner(
    root: &Path,
    round: &str,
    task: Option<&str>,
    attempt: Option<&str>,
    gate_identity: Option<GateAuditIdentity<'_>>,
    entry: GuardEntry,
    paths: &[PathBuf],
) -> Result<StoragePermit> {
    if entry.is_reclaim() {
        return Ok(StoragePermit {
            reservations: Vec::new(),
            entry,
            test_scratch_gate_lane: None,
        });
    }
    // Integration fixtures are independent repositories which may invoke the production gate
    // boundary concurrently in one libtest process.  A canonical direct-child check makes this a
    // narrow, conservative serialization lane: it never lowers the threshold or splits the real
    // per-filesystem reservation domain, and a symlink to a repository outside test scratch does
    // not qualify.
    let mut test_scratch_gate_lane = TestScratchGateLane::acquire(root, entry);
    let settings = StorageSettings::for_root(root)?;
    let probes = probe_filesystems(paths);
    let estimate = settings.estimate(entry);
    let threshold_bytes = settings.threshold.floor_bytes.saturating_add(estimate);
    let reason = first_probe_failure(&probes)
        .map(str::to_string)
        .or_else(|| {
            probes
                .is_empty()
                .then(|| "no filesystem probe target".to_string())
        });
    let decision = if probes.is_empty() {
        Err(GuardOutcome::Refuse {
            available_bytes: 0,
            threshold_bytes,
            entry,
            probe: "failed",
        })
    } else {
        // OUTSTANDING is process-local and keyed by physical filesystem in production. The
        // `cfg(test)` key additionally separates independent libtest fixture repositories.
        StoragePermit::acquire_filesystems(
            &settings.threshold,
            entry,
            Some(root),
            &probes,
            estimate,
        )
    };
    let decision = decision.map(|mut permit| {
        permit.test_scratch_gate_lane = test_scratch_gate_lane.take();
        permit
    });
    audit_admission(
        root,
        round,
        task,
        attempt,
        gate_identity,
        entry,
        settings,
        &probes,
        threshold_bytes,
        reason.as_deref(),
        decision,
    )
}

/// Recheck a gate immediately before its next child spawn while retaining the wider workflow's
/// original reservation. The refresh observes current free space, subtracts other permits, never
/// reserves this permit a second time, and rejects a filesystem absent from the original guard.
pub fn refresh_gate_permit(
    root: &Path,
    round: &str,
    identity: GateAuditIdentity<'_>,
    permit: &StoragePermit,
    paths: &[PathBuf],
) -> Result<()> {
    identity.validate()?;
    let entry = GuardEntry::Gate;
    let settings = StorageSettings::for_root(root)?;
    let probes = probe_filesystems(paths);
    let estimate = settings.estimate(entry);
    let threshold_bytes = settings.threshold.floor_bytes.saturating_add(estimate);
    let uncovered = probes.iter().find_map(|(fsid, result)| {
        matches!(result, ProbeResult::Ok { .. })
            .then_some(*fsid)
            .filter(|candidate| {
                let key = ReservationKey::new(Some(root), *candidate);
                !permit
                    .reservations
                    .iter()
                    .any(|(held_key, _)| held_key == &key)
            })
    });
    let reason = first_probe_failure(&probes)
        .map(str::to_string)
        .or_else(|| {
            uncovered.map(|fsid| format!("filesystem {fsid} is not covered by this permit"))
        })
        .or_else(|| {
            probes
                .is_empty()
                .then(|| "no filesystem probe target".to_string())
        });
    let decision = if probes.is_empty() {
        Err(GuardOutcome::Refuse {
            available_bytes: 0,
            threshold_bytes,
            entry,
            probe: "failed",
        })
    } else {
        permit.refresh_filesystems(&settings.threshold, entry, Some(root), &probes, estimate)
    };
    audit_admission(
        root,
        round,
        Some(identity.task_id()),
        identity.attempt_id(),
        Some(identity),
        entry,
        settings,
        &probes,
        threshold_bytes,
        reason.as_deref(),
        decision,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn maintenance_native_observer_distinguishes_existing_mounted_filesystems() {
        let root = crate::util::test_scratch_dir("maintenance-native-volumes");
        let other = PathBuf::from("/dev");
        let first = fs::metadata(&root).unwrap().dev();
        let second = fs::metadata(&other).unwrap().dev();
        assert_ne!(
            first, second,
            "this native multi-filesystem check requires distinct existing mounts"
        );
        let observations = observe_filesystem_space(&[root.clone(), other]);
        assert_eq!(observations.len(), 2);
        assert!(observations.iter().all(|o| o.error.is_none()));
        assert!(observations.iter().any(|o| o.device == first));
        assert!(observations.iter().any(|o| o.device == second));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostic_capacity_is_not_a_refreshable_gate_permit() {
        use super::*;
        let threshold = StorageThreshold::from_floor_bytes(8 * GIB);
        let probe = ProbeResult::Ok {
            available_bytes: 100 * GIB,
            total_bytes: 200 * GIB,
        };
        let permit = StoragePermit::acquire_filesystems(
            &threshold,
            GuardEntry::Diagnostic,
            None,
            &[(0xd1a60001, probe.clone())],
            38 * GIB,
        )
        .unwrap();
        assert!(permit
            .refresh_filesystems(
                &threshold,
                GuardEntry::Gate,
                None,
                &[(0xd1a60001, probe)],
                38 * GIB
            )
            .is_err());
        let root = crate::util::test_scratch_dir("diagnostic-budget-class");
        let settings = StorageSettings::for_root(&root).unwrap();
        assert_eq!(
            settings.estimate(GuardEntry::Diagnostic),
            settings.estimate(GuardEntry::Gate)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::*;

    #[test]
    fn test_scratch_gate_lane_accepts_only_canonical_direct_repositories() {
        use std::os::unix::fs::symlink;

        let direct = crate::util::test_scratch_dir("storage-gate-lane-direct");
        assert!(is_direct_test_scratch_repository(&direct));

        let nested = direct.join("nested");
        fs::create_dir_all(&nested).unwrap();
        assert!(!is_direct_test_scratch_repository(&nested));

        let link = direct
            .parent()
            .unwrap()
            .join(crate::util::unique_scratch_name(
                "storage-gate-lane-outside-link",
            ));
        symlink(&nested, &link).unwrap();
        assert!(!is_direct_test_scratch_repository(&link));

        fs::remove_file(link).unwrap();
        fs::remove_dir_all(direct).unwrap();
    }

    #[test]
    fn machine_values_can_only_tighten_defaults() {
        let defaults = StorageSettings::default();
        assert_eq!(defaults.threshold.floor_bytes(), DEFAULT_FLOOR_BYTES);
        assert!(defaults.audit_reserve_bytes >= 16 * MIB);
        assert_eq!(defaults.gate_estimate_bytes, 38 * GIB);
        assert_eq!(
            defaults
                .threshold
                .floor_bytes()
                .saturating_add(defaults.gate_estimate_bytes),
            DEFAULT_FLOOR_BYTES + DEFAULT_GATE_ESTIMATE_BYTES
        );
    }

    #[test]
    fn machine_gate_estimate_can_only_tighten_the_38_gib_default() {
        let root = crate::util::test_scratch_dir("storage-gate-machine-overlay");
        fs::create_dir_all(root.join(".orch")).unwrap();
        fs::write(
            root.join(".orch/machine.yaml"),
            format!(
                "storage:\n  floorBytes: {}\n  gateEstimateBytes: {}\n",
                GIB, GIB
            ),
        )
        .unwrap();
        let tightened = StorageSettings::for_root(&root).unwrap();
        assert_eq!(tightened.threshold.floor_bytes(), 8 * GIB);
        assert_eq!(tightened.gate_estimate_bytes, 38 * GIB);
        assert_eq!(
            tightened
                .threshold
                .floor_bytes()
                .saturating_add(tightened.gate_estimate_bytes),
            46 * GIB
        );
        fs::write(
            root.join(".orch/machine.yaml"),
            format!(
                "storage:\n  floorBytes: {}\n  gateEstimateBytes: {}\n",
                10 * GIB,
                40 * GIB
            ),
        )
        .unwrap();
        let tightened = StorageSettings::for_root(&root).unwrap();
        assert_eq!(tightened.threshold.floor_bytes(), 10 * GIB);
        assert_eq!(tightened.gate_estimate_bytes, 40 * GIB);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn held_gate_permit_refreshes_without_reserving_itself_twice() {
        let threshold = StorageThreshold::from_floor_bytes(8 * GIB);
        let fsid = 0xB263_0000_0000_0001;
        let probe = ProbeResult::Ok {
            available_bytes: 50 * GIB,
            total_bytes: 460 * GIB,
        };
        let permit = StoragePermit::acquire_filesystems(
            &threshold,
            GuardEntry::Gate,
            None,
            &[(fsid, probe.clone())],
            38 * GIB,
        )
        .expect("50 GiB must admit one 38 GiB estimate above the 8 GiB floor");
        permit
            .refresh_filesystems(
                &threshold,
                GuardEntry::Gate,
                None,
                &[(fsid, probe.clone())],
                38 * GIB,
            )
            .expect("refresh must subtract this permit from outstanding reservations");
        permit
            .refresh_filesystems(
                &threshold,
                GuardEntry::Gate,
                None,
                &[(fsid, probe)],
                38 * GIB,
            )
            .expect("repeated refresh must not add another reservation");
        assert_eq!(
            reservations()
                .lock()
                .unwrap()
                .get(&ReservationKey::new(None, fsid))
                .copied(),
            Some(38 * GIB),
            "the held permit must reserve its estimate exactly once"
        );
        let low = permit
            .refresh_filesystems(
                &threshold,
                GuardEntry::Gate,
                None,
                &[(
                    fsid,
                    ProbeResult::Ok {
                        available_bytes: 45 * GIB,
                        total_bytes: 460 * GIB,
                    },
                )],
                38 * GIB,
            )
            .expect_err("a later probe below 46 GiB must be observed");
        assert!(matches!(low, GuardOutcome::Refuse { probe: "low", .. }));
        drop(permit);
    }

    #[test]
    fn held_gate_permit_rejects_a_new_filesystem() {
        let threshold = StorageThreshold::from_floor_bytes(8 * GIB);
        let probe = ProbeResult::Ok {
            available_bytes: 50 * GIB,
            total_bytes: 460 * GIB,
        };
        let permit = StoragePermit::acquire_filesystems(
            &threshold,
            GuardEntry::Gate,
            None,
            &[(0xB263_0000_0000_0002, probe.clone())],
            38 * GIB,
        )
        .unwrap();
        let refusal = permit
            .refresh_filesystems(
                &threshold,
                GuardEntry::Gate,
                None,
                &[(0xB263_0000_0000_0003, probe)],
                38 * GIB,
            )
            .expect_err("a newly discovered filesystem is outside the original reservation");
        assert!(matches!(
            refusal,
            GuardOutcome::Refuse {
                probe: "uncovered",
                ..
            }
        ));
    }

    #[test]
    fn libtest_fixture_repositories_do_not_contend_on_one_host_fsid() {
        let threshold = StorageThreshold::from_floor_bytes(8 * GIB);
        let fsid = 0xB263_0000_0000_0004;
        let probe = ProbeResult::Ok {
            available_bytes: 50 * GIB,
            total_bytes: 460 * GIB,
        };
        let first = StoragePermit::acquire_filesystems(
            &threshold,
            GuardEntry::Gate,
            Some(Path::new("/fixture/repo-a")),
            &[(fsid, probe.clone())],
            38 * GIB,
        )
        .unwrap();
        assert!(
            StoragePermit::acquire_filesystems(
                &threshold,
                GuardEntry::Gate,
                Some(Path::new("/fixture/repo-a")),
                &[(fsid, probe.clone())],
                38 * GIB,
            )
            .is_err(),
            "two operations in the same fixture repository must share the fsid reservation"
        );
        let other_root = StoragePermit::acquire_filesystems(
            &threshold,
            GuardEntry::Gate,
            Some(Path::new("/fixture/repo-b")),
            &[(fsid, probe)],
            38 * GIB,
        )
        .expect("independent libtest fixture roots must not consume one another's test budget");
        drop((first, other_root));
    }

    #[test]
    fn gate_refusal_names_need_current_probe_and_recovery_command() {
        let message = storage_refusal_message(GuardEntry::Gate, "low", 45 * GIB, 46 * GIB, None);
        assert!(message.contains("需要 46 GiB"));
        assert!(message.contains("当前 45 GiB"));
        assert!(message.contains("`orch sites gc`"));
        assert!(message.contains(&format!("availableBytes={}", 45 * GIB)));
        assert!(message.contains(&format!("thresholdBytes={}", 46 * GIB)));
        assert!(message.contains("probeReason=none"));

        let failed = storage_refusal_message(
            GuardEntry::Gate,
            "failed",
            0,
            46 * GIB,
            Some("df unavailable"),
        );
        assert!(failed.contains("当前 未知"));
        assert!(failed.contains("probe=failed"));
        assert!(failed.contains("availableBytes=0"));
        assert!(failed.contains(&format!("thresholdBytes={}", 46 * GIB)));
        assert!(failed.contains("probeReason=df unavailable"));
    }

    #[test]
    fn gate_refusal_and_recovery_events_keep_machine_fields() {
        let refused = storage_refusal_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            "failed",
            0,
            46 * GIB,
            Some("df unavailable"),
        );
        let refused = refused.payload.expect("refusal payload");
        assert_eq!(refused["stage"], "storage");
        assert_eq!(refused["availableBytes"], 0);
        assert_eq!(refused["thresholdBytes"], 46 * GIB);
        assert_eq!(refused["entry"], "gate");
        assert_eq!(refused["probe"], "failed");
        assert_eq!(refused["probeReason"], "df unavailable");

        let recovered = storage_recovery_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        let recovered = recovered.payload.expect("recovery payload");
        assert_eq!(recovered["stage"], "storage");
        assert_eq!(recovered["state"], "recovered");
        assert_eq!(recovered["availableBytes"], 50 * GIB);
        assert_eq!(recovered["thresholdBytes"], 46 * GIB);
        assert_eq!(recovered["probe"], "ok");
        assert!(recovered["probeReason"].is_null());
    }

    #[test]
    fn verifier_storage_suffix_accepts_only_exact_gate_audit_shapes() {
        let refused = storage_refusal_event(
            Some("B-test"),
            "r-test",
            None,
            GuardEntry::Gate,
            "low",
            45 * GIB,
            46 * GIB,
            None,
        );
        assert!(ledger::canonical_gate_storage_audit_event(&refused)
            .is_some_and(|audit| { audit.round == "r-test" && audit.task_id == "B-test" }));

        let failed = storage_refusal_event(
            Some("B-test"),
            "r-test",
            None,
            GuardEntry::Gate,
            "failed",
            0,
            46 * GIB,
            Some("df unavailable"),
        );
        assert!(ledger::canonical_gate_storage_audit_event(&failed)
            .is_some_and(|audit| { audit.round == "r-test" && audit.task_id == "B-test" }));

        let recovered = storage_recovery_event(
            Some("B-test"),
            "r-test",
            None,
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        assert!(ledger::canonical_gate_storage_audit_event(&recovered)
            .is_some_and(|audit| { audit.round == "r-test" && audit.task_id == "B-test" }));

        for (label, mut forged) in [
            ("attempt", refused.clone()),
            ("entry", refused.clone()),
            ("threshold", refused.clone()),
            ("low value relation", refused.clone()),
            ("extra payload key", refused.clone()),
            ("extra top-level key", refused.clone()),
            ("failed nonzero available", failed.clone()),
            ("failed empty reason", failed.clone()),
            ("recovered low available", recovered.clone()),
            ("recovered non-null reason", recovered.clone()),
        ] {
            match label {
                "attempt" => forged.payload.as_mut().unwrap()["attemptId"] = serde_json::json!("A"),
                "entry" => {
                    forged.payload.as_mut().unwrap()["entry"] = serde_json::json!("dispatch")
                }
                "threshold" => {
                    forged.payload.as_mut().unwrap()["thresholdBytes"] = serde_json::json!(16 * GIB)
                }
                "low value relation" => {
                    forged.payload.as_mut().unwrap()["availableBytes"] = serde_json::json!(50 * GIB)
                }
                "extra payload key" => {
                    forged.payload.as_mut().unwrap()["forged"] = serde_json::json!(true)
                }
                "extra top-level key" => {
                    forged
                        .extra
                        .insert("forged".into(), serde_json::json!(true));
                }
                "failed nonzero available" => {
                    forged.payload.as_mut().unwrap()["availableBytes"] = serde_json::json!(GIB)
                }
                "failed empty reason" => {
                    forged.payload.as_mut().unwrap()["probeReason"] = serde_json::json!("")
                }
                "recovered low available" => {
                    forged.payload.as_mut().unwrap()["availableBytes"] = serde_json::json!(45 * GIB)
                }
                "recovered non-null reason" => {
                    forged.payload.as_mut().unwrap()["probeReason"] = serde_json::json!("stale")
                }
                _ => unreachable!(),
            }
            assert!(
                ledger::canonical_gate_storage_audit_event(&forged).is_none(),
                "forged {label} must fail closed"
            );
        }
    }

    #[test]
    fn gate_storage_pairing_is_exact_by_round_task_and_attempt_identity() {
        let a1 = GateAuditIdentity::Attempt {
            task_id: "B-test",
            attempt_id: "B-test-A0001",
        };
        let refusal = storage_refusal_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            "low",
            45 * GIB,
            46 * GIB,
            None,
        );
        let a2_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0002"),
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        let pre_attempt_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            None,
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        let exact_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );

        assert_eq!(
            ledger::gate_storage_pair_state(&[a2_recovery.clone()], "r-test", a1),
            ledger::GateStoragePairState::NeverSeen
        );
        assert_eq!(
            ledger::gate_storage_pair_state(
                &[refusal.clone(), a2_recovery, pre_attempt_recovery],
                "r-test",
                a1,
            ),
            ledger::GateStoragePairState::Refused
        );
        assert_eq!(
            ledger::gate_storage_pair_state(&[refusal, exact_recovery], "r-test", a1),
            ledger::GateStoragePairState::Recovered
        );
    }

    #[test]
    fn total_capacity_never_changes_the_absolute_decision() {
        let threshold = StorageThreshold::from_floor_bytes(20 * GIB);
        for total in [100 * GIB, 4_000 * GIB] {
            assert!(matches!(
                storage_guard_decision(
                    &threshold,
                    GuardEntry::Gate,
                    &ProbeResult::Ok {
                        available_bytes: 19 * GIB,
                        total_bytes: total,
                    }
                ),
                GuardOutcome::Refuse { probe: "low", .. }
            ));
        }
    }

    fn event(kind: &str, ts: &str, payload: serde_json::Value) -> EventRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": ulid::Ulid::new().to_string(),
            "ts": ts,
            "actor": "runtime:orch",
            "type": kind,
            "round": "r-test",
            "taskId": "B-test",
            "payload": payload,
        }))
        .unwrap()
    }

    #[test]
    fn interruption_fold_uses_latest_attempt_start_and_recovery_pair() {
        let events = vec![
            event(
                "EscalationRaised",
                "2026-08-03T00:00:00Z",
                serde_json::json!({"stage":"storage", "probe":"low"}),
            ),
            event(
                "AttemptStarted",
                "2026-08-03T00:01:00Z",
                serde_json::json!({"attemptId":"B-test-A0001"}),
            ),
            event(
                "EscalationRaised",
                "2026-08-03T00:02:00Z",
                serde_json::json!({"stage":"storage", "probe":"failed"}),
            ),
        ];
        assert!(storage_interruption_active(
            &events,
            "B-test",
            "B-test-A0001"
        ));
        let mut recovered = events.clone();
        recovered.push(event(
            "EscalationRaised",
            "2026-08-03T00:03:00Z",
            serde_json::json!({"stage":"storage", "state":"recovered"}),
        ));
        assert!(!storage_interruption_active(
            &recovered,
            "B-test",
            "B-test-A0001"
        ));
    }

    #[test]
    fn canonical_pre_attempt_or_other_attempt_cannot_recover_attempt_liveness() {
        let started = event(
            "AttemptStarted",
            "2026-08-03T00:00:00Z",
            serde_json::json!({"attemptId":"B-test-A0001"}),
        );
        let refusal = storage_refusal_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            "low",
            45 * GIB,
            46 * GIB,
            None,
        );
        let pre_attempt_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            None,
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        let other_attempt_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0002"),
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        let mut cross_round_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        cross_round_recovery.round = Some("r-other".to_string());
        let exact_recovery = storage_recovery_event(
            Some("B-test"),
            "r-test",
            Some("B-test-A0001"),
            GuardEntry::Gate,
            Some(50 * GIB),
            46 * GIB,
            None,
        );
        let mut malformed_same_attempt_recovery = exact_recovery.clone();
        malformed_same_attempt_recovery
            .extra
            .insert("mergeSha".to_string(), serde_json::json!("smuggled"));

        let mut events = vec![started, refusal];
        events.push(pre_attempt_recovery);
        assert!(storage_interruption_active(
            &events,
            "B-test",
            "B-test-A0001"
        ));
        events.push(other_attempt_recovery);
        assert!(storage_interruption_active(
            &events,
            "B-test",
            "B-test-A0001"
        ));
        events.push(cross_round_recovery);
        assert!(storage_interruption_active(
            &events,
            "B-test",
            "B-test-A0001"
        ));
        events.push(malformed_same_attempt_recovery);
        assert!(storage_interruption_active(
            &events,
            "B-test",
            "B-test-A0001"
        ));
        events.push(exact_recovery);
        assert!(!storage_interruption_active(
            &events,
            "B-test",
            "B-test-A0001"
        ));
    }

    #[test]
    fn audit_reserve_is_real_allocated_bytes_not_set_len_sparse() {
        let root = crate::util::test_scratch_dir("storage-audit-reserve");
        ensure_audit_reserve(&root, 64 * 1024).unwrap();
        let metadata = fs::metadata(root.join(AUDIT_RESERVE_REL)).unwrap();
        assert_eq!(metadata.len(), 64 * 1024);
        assert!(metadata.blocks().saturating_mul(512) >= 64 * 1024);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_paths_on_one_filesystem_produce_one_probe() {
        let root = crate::util::test_scratch_dir("storage-probe-dedup");
        fs::create_dir_all(root.join("nested")).unwrap();
        let probes = probe_filesystems(&[root.clone(), root.join("nested")]);
        assert_eq!(probes.len(), 1, "same fsid must be probed exactly once");
        fs::remove_dir_all(root).unwrap();
    }

    fn position_after(source: &str, start: &str, needle: &str) -> usize {
        let start = source.find(start).expect("entry function missing");
        start
            + source[start..]
                .find(needle)
                .expect("entry operation missing")
    }

    #[test]
    fn production_guards_precede_each_entrys_first_effect_boundary() {
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let tierf = fs::read_to_string(source_root.join("tierf.rs")).unwrap();
        let dispatch = "fn run_dispatch_with_hook_inner<F>";
        assert!(
            position_after(&tierf, dispatch, "crate::storage::guard_operation")
                < position_after(&tierf, dispatch, "crate::channel::with_capacity_lock")
        );

        let wake = fs::read_to_string(source_root.join("wake.rs")).unwrap();
        let review = "pub fn run_wake_with_message_authorized_review";
        assert!(
            position_after(&wake, review, "crate::storage::guard_operation")
                < position_after(&wake, review, "run_wake_with_message_mode")
        );

        let collect = fs::read_to_string(source_root.join("collect.rs")).unwrap();
        let gate = "pub fn check_and_gate";
        assert!(
            position_after(&collect, gate, "crate::storage::guard_gate_operation")
                < position_after(&collect, gate, "mech::check")
        );
    }

    #[test]
    fn refusal_fold_deduplicates_until_one_recovery_pair() {
        let low = event(
            "EscalationRaised",
            "2026-08-03T00:00:00Z",
            serde_json::json!({"stage":"storage", "probe":"low"}),
        );
        assert!(storage_refusal_active(&[low.clone()], Some("B-test"), None));
        let recovered = event(
            "EscalationRaised",
            "2026-08-03T00:01:00Z",
            serde_json::json!({"stage":"storage", "state":"recovered"}),
        );
        assert!(!storage_refusal_active(
            &[low, recovered],
            Some("B-test"),
            None
        ));
    }
}
