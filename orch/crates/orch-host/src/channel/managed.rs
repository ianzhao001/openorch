//! Shared managed-process transport and immutable invocation control.
//! Task, round, review-site and ledger authority live in the selfhost child;
//! default builds compile the same process identity, ACK, terminal and custody kernel.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
#[cfg(feature = "selfhost")]
use std::time::{SystemTime, UNIX_EPOCH};
use anyhow::{bail, Context, Result};
use wait_timeout::ChildExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(feature = "selfhost")]
use crate::harness::WRAPPER_EXIT_CODES;


#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
    fn setsid() -> i32;
    fn getuid() -> u32;
    #[cfg(test)]
    fn getpgrp() -> i32;
}

/// Runtime-owned durable identity, selected from the provider's actual process
/// topology rather than from the short-lived wait script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "selfhost")]
pub enum DurableIdentityKind {
    /// A directly managed CLI child. Its spawn PID is also its process-group id.
    ManagedPidGroup,
    /// A socket bridge whose backend is not a descendant of the spawned wrapper.
    /// Progress in the exact per-attempt wake log is the only honest proxy.
    WakeLogProxy,
}

#[cfg(feature = "selfhost")]
impl DurableIdentityKind {
    fn is_managed_pid_group(self) -> bool {
        matches!(self, Self::ManagedPidGroup)
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOwnerKind {
    ManagedSupervisor,
    #[cfg(feature = "selfhost")]
    WakeLogProxy,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOwnerHandoff {
    AfterSupervisorAck,
    #[cfg(feature = "selfhost")]
    ImmediatelyAfterSpawn,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeOwnerLifecyclePlan {
    pub isolate_process_group: bool,
    pub prearm_reaper: bool,
    pub handoff: WakeOwnerHandoff,
}

#[doc(hidden)]
pub fn wake_owner_lifecycle_plan(kind: WakeOwnerKind) -> WakeOwnerLifecyclePlan {
    WakeOwnerLifecyclePlan {
        isolate_process_group: true,
        prearm_reaper: true,
        handoff: match kind {
            WakeOwnerKind::ManagedSupervisor => WakeOwnerHandoff::AfterSupervisorAck,
            #[cfg(feature = "selfhost")]
            WakeOwnerKind::WakeLogProxy => WakeOwnerHandoff::ImmediatelyAfterSpawn,
        },
    }
}

/// Provider-frame classification used to start the managed natural-exit grace;
/// it does not by itself prove process termination or an eligible review answer.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedTerminal {
    /// No recognized completion frame; continue supervising the provider.
    Continue,
    /// A recognized completion frame from a supported managed driver, including
    /// strict Claude/CodeBuddy success. The name preserves source/API compatibility.
    OpenCodeStop,
}

/// Classify one LF-complete JSON record. The caller, rather than this parser,
/// owns line framing so an unterminated tail can never become terminal.
/// Recognized driver completions share `OpenCodeStop`; Claude/CodeBuddy require
/// the same nonempty, error-free success-result grammar as receipt extraction.
#[doc(hidden)]
pub fn classify_managed_terminal(program: &str, complete_json_line: &str) -> ManagedTerminal {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(complete_json_line) else {
        return ManagedTerminal::Continue;
    };
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str());
    let opencode_stop = basename == Some("opencode")
        && value.get("type").and_then(|v| v.as_str()) == Some("step_finish")
        && value
            .get("part")
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            == Some("step-finish")
        && value
            .get("part")
            .and_then(|v| v.get("reason"))
            .and_then(|v| v.as_str())
            == Some("stop");
    let codex_stop = basename == Some("codex")
        && value.get("type").and_then(|v| v.as_str()) == Some("turn.completed");
    let claude_family_stop = matches!(basename, Some("claude" | "codebuddy"))
        && claude_family_result_is_success(&value);
    let managed_wrapper_stop = matches!(
        value.get("type").and_then(|v| v.as_str()),
        Some("pi.terminal" | "zcode.terminal" | "dsh.terminal")
    ) && value
        .get("sessionId")
        .and_then(|session| session.as_str())
        .is_some_and(|session| !session.trim().is_empty());
    if opencode_stop || codex_stop || claude_family_stop || managed_wrapper_stop {
        ManagedTerminal::OpenCodeStop
    } else {
        ManagedTerminal::Continue
    }
}

fn claude_family_result_is_success(value: &serde_json::Value) -> bool {
    value.get("type").and_then(serde_json::Value::as_str) == Some("result")
        && value.get("subtype").and_then(serde_json::Value::as_str) == Some("success")
        && value.get("is_error").and_then(serde_json::Value::as_bool) == Some(false)
        && value
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|result| !result.trim().is_empty())
        && match value.get("error") {
            None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false)) => true,
            _ => false,
        }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeSupervisorPolicy {
    pub natural_exit_grace_ms: u64,
    pub term_grace_ms: u64,
    pub poll_interval_ms: u64,
}

impl WakeSupervisorPolicy {
    fn production() -> Self {
        Self {
            natural_exit_grace_ms: 3_000,
            term_grace_ms: 2_000,
            poll_interval_ms: 20,
        }
    }
}

/// Finite safety ceiling for every directly managed provider child.
#[doc(hidden)]
pub const MANAGED_WAKE_MAX_RUNTIME_SECS: u64 = 21_600;
#[cfg(feature = "selfhost")]
const REVIEW_DEADLINE_BASE_SECS: u64 = 1_800;
#[cfg(feature = "selfhost")]
const REVIEW_EVIDENCE_ITEM_SECS: u64 = 300;
#[cfg(feature = "selfhost")]
const PRIMARY_REVIEW_TIME_FACTOR: u64 = 2;

/// Derive a review SLA from the declared role and evidence workload.
///
/// The arithmetic is deliberately saturating before applying the managed-wake
/// ceiling: even an adversarial `usize::MAX` workload can only lengthen the
/// deadline to the finite supervisor limit, never wrap it below the old
/// half-hour default.
#[cfg(feature = "selfhost")]
pub fn review_deadline_secs(role: &str, required_evidence_items: usize) -> u64 {
    let items = u64::try_from(required_evidence_items).unwrap_or(u64::MAX);
    let role_factor = if role == "primary" {
        PRIMARY_REVIEW_TIME_FACTOR
    } else {
        1
    };
    REVIEW_DEADLINE_BASE_SECS
        .saturating_add(REVIEW_EVIDENCE_ITEM_SECS.saturating_mul(items))
        .saturating_mul(role_factor)
        .min(MANAGED_WAKE_MAX_RUNTIME_SECS)
}

pub const ZCODE_STREAM_SCRIPT_REL: &str = "orch/scripts/wake-zcode-stream.sh";
pub const PI_STREAM_SCRIPT_REL: &str = "orch/scripts/wake-pi-stream.sh";
const DSH_STREAM_SCRIPT_REL: &str = "orch/scripts/wake-dsh-stream.sh";

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedWakeRuntimeSource {
    OrdinaryDefault,
    ReviewDeadline,
    ReviewDeadlineDisabledFallback,
    ReviewDeadlineCapped,
}

impl ManagedWakeRuntimeSource {
    #[cfg(feature = "selfhost")]
    fn ledger_name(self) -> &'static str {
        match self {
            Self::OrdinaryDefault => "ordinary-default",
            Self::ReviewDeadline => "review-deadline",
            Self::ReviewDeadlineDisabledFallback => "review-deadline-disabled-fallback",
            Self::ReviewDeadlineCapped => "review-deadline-capped",
        }
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedWakeRuntimeLimit {
    pub requested_review_deadline_secs: Option<u64>,
    pub effective_secs: u64,
    pub source: ManagedWakeRuntimeSource,
}

/// An attach continuation is always an explicit operator choice.  Resume and
/// fork deliberately occupy different durable action identities so a failed
/// resume can never silently turn into a fork.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttachMode {
    /// Requested continuation of the same native session; currently unsupported.
    Resume,
    /// Requested independent fork of native state; currently unsupported.
    Fork,
}

impl AttachMode {
    /// Stable spelling in historical attach records and explicit CLI requests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Fork => "fork",
        }
    }
}

impl std::fmt::Display for AttachMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Accept exactly one LF-complete OpenCode `step_start` object with one
/// top-level `sessionID`.  A recursive count rejects nested copies and
/// top-level/nested conflicts rather than applying first/last-wins semantics.
pub fn classify_opencode_session_receipt(complete_json_object: &str) -> Option<String> {
    if complete_json_object.is_empty()
        || complete_json_object.trim() != complete_json_object
        || complete_json_object
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return None;
    }
    // `serde_json::Value` intentionally applies last-wins semantics to duplicate
    // object keys.  Reject anything other than one literal spelling before
    // deserializing so duplicate top-level keys cannot silently replace the
    // authenticated session identity.  Escaped key spellings and occurrences in
    // string values also fail closed rather than widening this receipt grammar.
    if complete_json_object.matches("\"sessionID\"").count() != 1 {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(complete_json_object).ok()?;
    let object = value.as_object()?;
    if object.get("type").and_then(serde_json::Value::as_str) != Some("step_start") {
        return None;
    }
    fn session_id_key_count(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(object) => object
                .iter()
                .map(|(key, child)| usize::from(key == "sessionID") + session_id_key_count(child))
                .sum(),
            serde_json::Value::Array(values) => values.iter().map(session_id_key_count).sum(),
            _ => 0,
        }
    }
    if session_id_key_count(&value) != 1 {
        return None;
    }
    let session_id = object.get("sessionID")?.as_str()?;
    if session_id.is_empty()
        || session_id.len() > 256
        || session_id.bytes().any(|byte| {
            byte.is_ascii_control()
                || byte.is_ascii_whitespace()
                || matches!(byte, b'/' | b'\\' | b':' | b'\0')
        })
        || !session_id.is_ascii()
    {
        return None;
    }
    Some(session_id.to_string())
}

#[cfg(feature = "selfhost")]
pub fn attach_action_id(
    round: &str,
    attempt_id: &str,
    role: &str,
    source_wake_id: &str,
    mode: AttachMode,
) -> String {
    sha256_hex(
        format!(
            "managed-wake-attach\0{round}\0{attempt_id}\0{role}\0{source_wake_id}\0{}",
            mode.as_str()
        )
        .as_bytes(),
    )
}

#[doc(hidden)]
pub fn managed_wake_runtime_limit(review_deadline_secs: Option<u64>) -> ManagedWakeRuntimeLimit {
    match review_deadline_secs {
        None => ManagedWakeRuntimeLimit {
            requested_review_deadline_secs: None,
            effective_secs: MANAGED_WAKE_MAX_RUNTIME_SECS,
            source: ManagedWakeRuntimeSource::OrdinaryDefault,
        },
        Some(0) => ManagedWakeRuntimeLimit {
            requested_review_deadline_secs: Some(0),
            effective_secs: MANAGED_WAKE_MAX_RUNTIME_SECS,
            source: ManagedWakeRuntimeSource::ReviewDeadlineDisabledFallback,
        },
        Some(requested) if requested <= MANAGED_WAKE_MAX_RUNTIME_SECS => ManagedWakeRuntimeLimit {
            requested_review_deadline_secs: Some(requested),
            effective_secs: requested,
            source: ManagedWakeRuntimeSource::ReviewDeadline,
        },
        Some(requested) => ManagedWakeRuntimeLimit {
            requested_review_deadline_secs: Some(requested),
            effective_secs: MANAGED_WAKE_MAX_RUNTIME_SECS,
            source: ManagedWakeRuntimeSource::ReviewDeadlineCapped,
        },
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedWakeStopReason {
    NaturalExit,
    HardDeadline,
    ManualCancel,
    ExactTerminal,
    OperationalError,
}

impl ManagedWakeStopReason {
    fn ledger_name(self) -> &'static str {
        match self {
            Self::NaturalExit => "natural-exit",
            Self::HardDeadline => "hard-deadline",
            Self::ManualCancel => "manual-cancel",
            Self::ExactTerminal => "exact-terminal",
            Self::OperationalError => "operational-error",
        }
    }
}

#[doc(hidden)]
pub fn select_managed_wake_stop_winner(
    current: Option<ManagedWakeStopReason>,
    provider_exited: bool,
    hard_deadline_reached: bool,
    authenticated_cancel: bool,
    terminal_grace_elapsed: bool,
) -> Option<ManagedWakeStopReason> {
    current.or_else(|| {
        if provider_exited {
            Some(ManagedWakeStopReason::NaturalExit)
        } else if hard_deadline_reached {
            Some(ManagedWakeStopReason::HardDeadline)
        } else if authenticated_cancel {
            Some(ManagedWakeStopReason::ManualCancel)
        } else if terminal_grace_elapsed {
            Some(ManagedWakeStopReason::ExactTerminal)
        } else {
            None
        }
    })
}

#[doc(hidden)]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedWakeCancelRequest {
    pub version: u32,
    pub wake_id: String,
    pub token: String,
    pub request_id: String,
    pub reason: String,
}

/// Check the canonical managed identity syntax; this alone proves no authority.
pub(crate) fn is_strict_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
        && value.as_bytes()[14] == b'4'
        && matches!(value.as_bytes()[19], b'8' | b'9' | b'a' | b'b')
}

fn is_lower_hex_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_cancel_reason(reason: &str) -> std::result::Result<(), String> {
    if reason.is_empty() || reason.len() > 512 {
        return Err("managed wake cancel reason must be 1..=512 UTF-8 bytes".to_string());
    }
    if reason.chars().next().is_some_and(char::is_whitespace)
        || reason.chars().last().is_some_and(char::is_whitespace)
    {
        return Err("managed wake cancel reason has framing whitespace".to_string());
    }
    if reason
        .bytes()
        .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n' || byte.is_ascii_control())
    {
        return Err("managed wake cancel reason contains ASCII control bytes".to_string());
    }
    Ok(())
}

#[doc(hidden)]
pub fn parse_managed_wake_cancel_request(
    bytes: &[u8],
    expected_wake_id: &str,
    expected_token: &str,
) -> std::result::Result<ManagedWakeCancelRequest, String> {
    if !is_strict_uuid(expected_wake_id) {
        return Err("expected managed wakeId is not a strict UUID".to_string());
    }
    if !is_lower_hex_token(expected_token) {
        return Err("expected managed wake token is not 32 lowercase hex".to_string());
    }
    let request: ManagedWakeCancelRequest = serde_json::from_slice(bytes)
        .map_err(|error| format!("managed wake cancel request is not exact typed JSON: {error}"))?;
    if request.version != 1 {
        return Err("managed wake cancel request version must be 1".to_string());
    }
    if request.wake_id != expected_wake_id || !is_strict_uuid(&request.wake_id) {
        return Err("managed wake cancel request wakeId mismatch".to_string());
    }
    if request.token != expected_token || !is_lower_hex_token(&request.token) {
        return Err("managed wake cancel request token mismatch".to_string());
    }
    if !is_strict_uuid(&request.request_id) {
        return Err("managed wake cancel request requestId is not a strict UUID".to_string());
    }
    validate_cancel_reason(&request.reason)?;
    Ok(request)
}

/// The strongest containment proof implemented by the selected backend.
///
/// Platform names alone never grant fork-complete authority.  A future
/// privileged backend must opt in explicitly after it has captured a
/// kernel-backed provenance boundary.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedContainmentCapability {
    ObservedReceipted,
    ForkComplete,
}

/// The honest claim derived from a backend capability and the observed
/// managed-scope cleanup fact.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedContainmentClaim {
    ContainmentUnproven,
    ManagedScopeTerminated,
    ForkCompleteTerminated,
}

impl ManagedContainmentClaim {
    pub fn fork_complete(self) -> bool {
        matches!(self, Self::ForkCompleteTerminated)
    }
}

/// Current production backends prove only processes that were directly
/// owned, promoted into immutable receipts, or covered by a fresh exact group
/// proof.  Darwin and Linux therefore share the same conservative answer;
/// unknown names fail closed to that answer as well.
#[doc(hidden)]
pub fn managed_containment_capability_for_platform(
    _platform: &str,
) -> ManagedContainmentCapability {
    ManagedContainmentCapability::ObservedReceipted
}

#[doc(hidden)]
pub fn managed_containment_claim(
    capability: ManagedContainmentCapability,
    managed_scope_terminated: bool,
) -> ManagedContainmentClaim {
    if !managed_scope_terminated {
        return ManagedContainmentClaim::ContainmentUnproven;
    }
    match capability {
        ManagedContainmentCapability::ObservedReceipted => {
            ManagedContainmentClaim::ManagedScopeTerminated
        }
        ManagedContainmentCapability::ForkComplete => {
            ManagedContainmentClaim::ForkCompleteTerminated
        }
    }
}

/// Canonical containment bundle.  Outcome and sidecar construction both flow
/// through this normalizer so legacy and typed fields cannot be populated
/// independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NormalizedManagedContainment {
    capability: ManagedContainmentCapability,
    claim: ManagedContainmentClaim,
    managed_scope_terminated: bool,
    fork_complete: bool,
    legacy_process_tree_terminated: bool,
}

fn normalize_managed_containment(
    capability: ManagedContainmentCapability,
    managed_scope_terminated: bool,
) -> NormalizedManagedContainment {
    let claim = managed_containment_claim(capability, managed_scope_terminated);
    let fork_complete = claim.fork_complete();
    NormalizedManagedContainment {
        capability,
        claim,
        managed_scope_terminated,
        fork_complete,
        // The status-sidecar compatibility field historically read as full
        // process-tree containment.  It may be true only for that exact claim.
        legacy_process_tree_terminated: fork_complete,
    }
}

fn production_managed_containment(managed_scope_terminated: bool) -> NormalizedManagedContainment {
    normalize_managed_containment(
        managed_containment_capability_for_platform(std::env::consts::OS),
        managed_scope_terminated,
    )
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeSupervisorOutcome {
    pub terminal_seen: bool,
    pub exited_naturally: bool,
    pub signals: Vec<super::process::TerminationSignal>,
    /// Frozen B177 ABI name.  Its precise meaning is managed-scope cleanup:
    /// direct Child custody plus observed immutable receipts/exact groups.
    /// It does not mean arbitrary unobserved OS descendants were terminated.
    pub process_tree_terminated: bool,
    pub containment_claim: ManagedContainmentClaim,
    pub hard_deadline_reached: bool,
    pub completion_reason: ManagedWakeStopReason,
}

impl WakeSupervisorOutcome {
    fn canonical(
        terminal_seen: bool,
        exited_naturally: bool,
        signals: Vec<super::process::TerminationSignal>,
        managed_scope_terminated: bool,
    ) -> Self {
        let containment = production_managed_containment(managed_scope_terminated);
        Self {
            terminal_seen,
            exited_naturally,
            signals,
            process_tree_terminated: containment.managed_scope_terminated,
            containment_claim: containment.claim,
            hard_deadline_reached: false,
            completion_reason: if exited_naturally {
                ManagedWakeStopReason::NaturalExit
            } else if terminal_seen {
                ManagedWakeStopReason::ExactTerminal
            } else {
                ManagedWakeStopReason::OperationalError
            },
        }
    }

    fn set_managed_scope_terminated(&mut self, managed_scope_terminated: bool) {
        let containment = production_managed_containment(managed_scope_terminated);
        self.process_tree_terminated = containment.managed_scope_terminated;
        self.containment_claim = containment.claim;
    }

    fn normalized_containment(&self) -> NormalizedManagedContainment {
        production_managed_containment(self.process_tree_terminated)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedProcessCredential {
    pub pid: u32,
    pub observed_ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub uid: u32,
    /// macOS: start sec/usec; Linux: /proc start ticks.
    pub birth_identity: String,
    /// SHA-256 over executable path plus stable filesystem identity. Never argv.
    pub executable_summary: String,
}

const ZOMBIE_PROVIDER_EPOCH_EXECUTABLE: &str = "orch:zombie-only-provider-epoch";

fn is_zombie_provider_epoch(receipt: &ManagedProcessCredential) -> bool {
    receipt.executable_summary == ZOMBIE_PROVIDER_EPOCH_EXECUTABLE
}

/// Cheap process-table row used for ancestry and group discovery.  It is
/// intentionally insufficient to authorize a signal: only promoted
/// `ManagedProcessCredential` values may do that.  Keeping executable hashing
/// out of this type is what makes a 20ms supervision tick independent of the
/// total process count.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedProcessTopology {
    pub pid: u32,
    pub observed_ppid: u32,
    pub pgid: u32,
    pub sid: u32,
    pub uid: u32,
    pub birth_identity: String,
    /// Kernel process state, used only to prove that an already-owned member
    /// is an unreaped zombie when executable inspection is unavailable.
    #[serde(default)]
    pub zombie: bool,
    /// Kernel-provided executable name, never argv and never a filesystem
    /// digest.  The full path/filesystem identity is captured only when a
    /// candidate is promoted.
    pub executable_hint: String,
}

/// The ownership proof for one exact live member of a managed process group.
///
/// This classification is deliberately only the topology half of signal
/// authority. Production callers must still bind a contained member's stable
/// credential to the A/full/B topology sandwich before acting on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberContainment {
    OwnedDirect,
    ContainedBySession,
    ContainedByLineage,
    Unowned,
}

/// Classify a live process-group member without consulting mutable process
/// state. Merely sharing a PGID is never containment: a process can migrate
/// into a sibling group with `setpgid(2)`. A non-session-isolated member must
/// instead name an already-owned parent and retain the leader's SID/UID.
#[allow(clippy::too_many_arguments)]
pub fn classify_group_member(
    member_pid: u32,
    member_ppid: u32,
    member_pgid: u32,
    member_sid: u32,
    member_uid: u32,
    leader_pid: u32,
    leader_sid: u32,
    leader_uid: u32,
    owned_pids: &BTreeSet<u32>,
) -> MemberContainment {
    if owned_pids.contains(&member_pid) {
        return MemberContainment::OwnedDirect;
    }
    if !owned_pids.contains(&leader_pid)
        || member_pid == leader_pid
        || member_pgid != leader_pid
        || member_sid != leader_sid
        || member_uid != leader_uid
    {
        return MemberContainment::Unowned;
    }
    if leader_sid == leader_pid {
        return MemberContainment::ContainedBySession;
    }
    if owned_pids.contains(&member_ppid) {
        MemberContainment::ContainedByLineage
    } else {
        MemberContainment::Unowned
    }
}

const RESIDUAL_CONVERGENCE_BUDGET: Duration = Duration::from_secs(5);

/// Whether the production residual-cleanup budget has been consumed.
pub fn convergence_budget_exhausted(elapsed: Duration) -> bool {
    elapsed >= RESIDUAL_CONVERGENCE_BUDGET
}

/// Stable, machine-distinguishable evidence for an ownership-proof give-up.
pub fn convergence_giveup_message(pgid: u32, unresolved: &[u32]) -> String {
    let unresolved = unresolved
        .iter()
        .copied()
        .filter(|pid| *pid != 0)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|pid| pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("residual-convergence-budget-exhausted: pgid={pgid}; unresolvedPids=[{unresolved}]")
}

/// Parse the complete canonical residual-convergence suffix emitted from the
/// supervisor's final `BTreeMap`. Earlier diagnostic text is ignored, but the
/// suffix itself must be byte-canonical, sorted, unique, and fully consumed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpaqueZombieProof {
    pid: u32,
    observed_ppid: u32,
    pgid: u32,
    uid: u32,
    birth_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ZombieContainmentReceipt {
    pid: u32,
    observed_ppid: u32,
    pgid: u32,
    /// `None` is an intentionally unknown SID from a birth-bearing opaque
    /// Darwin zombie row. It is containment-only and never seeds lineage or
    /// signal authority.
    sid: Option<u32>,
    uid: u32,
    birth_identity: String,
}

struct ProcessTopologyCensus {
    /// Exact positive unique PIDs declared by the platform census before any
    /// per-process detail race. A declared PID without detail is ambiguity,
    /// never disappearance.
    listed_pids: BTreeSet<u32>,
    topology: BTreeMap<u32, ManagedProcessTopology>,
    opaque_zombies: BTreeMap<u32, OpaqueZombieProof>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ZombieEpochObservation {
    pid: u32,
    observed_ppid: u32,
    pgid: u32,
    sid: Option<u32>,
    uid: u32,
    birth_identity: String,
}

impl ZombieEpochObservation {
    fn from_topology(topology: &ManagedProcessTopology) -> Result<Option<Self>> {
        if !topology.zombie {
            return Ok(None);
        }
        if topology.pid == 0 || topology.pgid == 0 || topology.birth_identity.is_empty() {
            bail!("full zombie topology lacks positive pid/pgid/birth identity");
        }
        Ok(Some(Self {
            pid: topology.pid,
            observed_ppid: topology.observed_ppid,
            pgid: topology.pgid,
            sid: (topology.sid != 0).then_some(topology.sid),
            uid: topology.uid,
            birth_identity: topology.birth_identity.clone(),
        }))
    }

    fn from_opaque(zombie: &OpaqueZombieProof) -> Result<Self> {
        let birth_identity = zombie
            .birth_identity
            .clone()
            .context("opaque zombie lacks birth identity")?;
        if zombie.pid == 0 || zombie.pgid == 0 || birth_identity.is_empty() {
            bail!("opaque zombie lacks positive pid/pgid/birth identity");
        }
        Ok(Self {
            pid: zombie.pid,
            observed_ppid: zombie.observed_ppid,
            pgid: zombie.pgid,
            sid: None,
            uid: zombie.uid,
            birth_identity,
        })
    }

    fn same_epoch_and_structure(&self, other: &Self) -> bool {
        self.pid == other.pid
            && self.observed_ppid == other.observed_ppid
            && self.pgid == other.pgid
            && self.uid == other.uid
            && self.birth_identity == other.birth_identity
            && match (self.sid, other.sid) {
                (Some(first), Some(second)) => first == second,
                _ => true,
            }
    }

    fn into_receipt(self, other: &Self) -> ZombieContainmentReceipt {
        ZombieContainmentReceipt {
            pid: self.pid,
            observed_ppid: self.observed_ppid,
            pgid: self.pgid,
            sid: self.sid.or(other.sid),
            uid: self.uid,
            birth_identity: self.birth_identity,
        }
    }
}

fn normalized_zombie_epoch(
    census: &ProcessTopologyCensus,
    pid: u32,
) -> Result<Option<ZombieEpochObservation>> {
    let full = census
        .topology
        .get(&pid)
        .map(ZombieEpochObservation::from_topology)
        .transpose()?
        .flatten();
    let opaque = census
        .opaque_zombies
        .get(&pid)
        .map(ZombieEpochObservation::from_opaque)
        .transpose()?;
    match (full, opaque) {
        (Some(full), Some(opaque)) => {
            if !full.same_epoch_and_structure(&opaque) {
                bail!("full and opaque zombie representations disagree for pid {pid}");
            }
            Ok(Some(ZombieEpochObservation {
                sid: full.sid.or(opaque.sid),
                ..full
            }))
        }
        (Some(full), None) => Ok(Some(full)),
        (None, Some(opaque)) => {
            if census.topology.get(&pid).is_some_and(|row| !row.zombie) {
                bail!("live topology conflicts with opaque zombie for pid {pid}");
            }
            Ok(Some(opaque))
        }
        (None, None) => Ok(None),
    }
}

fn normalized_zombie_epochs(
    census: &ProcessTopologyCensus,
) -> Result<BTreeMap<u32, ZombieEpochObservation>> {
    let pids = census
        .topology
        .keys()
        .chain(census.opaque_zombies.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut result = BTreeMap::new();
    for pid in pids {
        if let Some(epoch) = normalized_zombie_epoch(census, pid)? {
            result.insert(pid, epoch);
        }
    }
    Ok(result)
}

fn stable_containment_creation(
    first: &ZombieEpochObservation,
    second: &ZombieEpochObservation,
    provider_pid: u32,
    _isolated_sid: u32,
    provider_uid: u32,
) -> Option<ZombieContainmentReceipt> {
    (first.same_epoch_and_structure(second)
        && first.pid != provider_pid
        && first.pgid != provider_pid
        && first.observed_ppid == provider_pid
        && second.observed_ppid == provider_pid
        && first.uid == provider_uid
        && second.uid == provider_uid)
        .then(|| first.clone().into_receipt(second))
}

fn stable_pending_containment_epoch(
    receipt: &ZombieContainmentReceipt,
    first: &ZombieEpochObservation,
    second: &ZombieEpochObservation,
    target_pgid: u32,
) -> bool {
    first.same_epoch_and_structure(second)
        && first.pid == receipt.pid
        && first.pgid == target_pgid
        && first.birth_identity == receipt.birth_identity
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CustodyEpochState {
    PresentExact,
    PresentStructuralConflict,
    PresentNonZombieConflict,
    Replaced(String),
    MissingDetail,
    Absent,
}

fn classify_custody_epoch(
    receipt: &ZombieContainmentReceipt,
    census: &ProcessTopologyCensus,
) -> Result<CustodyEpochState> {
    if let Some(epoch) = normalized_zombie_epoch(census, receipt.pid)? {
        if epoch.birth_identity != receipt.birth_identity {
            return Ok(CustodyEpochState::Replaced(epoch.birth_identity));
        }
        let structure_matches = epoch.observed_ppid == receipt.observed_ppid
            && epoch.pgid == receipt.pgid
            && epoch.uid == receipt.uid
            && match (receipt.sid, epoch.sid) {
                (Some(expected), Some(current)) => expected == current,
                _ => true,
            };
        return Ok(if structure_matches {
            CustodyEpochState::PresentExact
        } else {
            CustodyEpochState::PresentStructuralConflict
        });
    }
    if let Some(topology) = census.topology.get(&receipt.pid) {
        if topology.birth_identity.is_empty() {
            return Ok(CustodyEpochState::MissingDetail);
        }
        return Ok(if topology.birth_identity == receipt.birth_identity {
            CustodyEpochState::PresentNonZombieConflict
        } else {
            CustodyEpochState::Replaced(topology.birth_identity.clone())
        });
    }
    if census.listed_pids.contains(&receipt.pid) {
        Ok(CustodyEpochState::MissingDetail)
    } else {
        Ok(CustodyEpochState::Absent)
    }
}

fn custody_epoch_retired_by_stable_pair(
    receipt: &ZombieContainmentReceipt,
    before: &ProcessTopologyCensus,
    after: &ProcessTopologyCensus,
) -> Result<bool> {
    let first = classify_custody_epoch(receipt, before)?;
    let second = classify_custody_epoch(receipt, after)?;
    match (&first, &second) {
        (CustodyEpochState::Absent, CustodyEpochState::Absent) => Ok(true),
        (CustodyEpochState::Replaced(first), CustodyEpochState::Replaced(second))
            if first == second =>
        {
            Ok(true)
        }
        // Every other pair preserves custody. Structural disagreement,
        // declared-without-detail, and an A/B transition all revoke any
        // current structural proof but can never prove the old epoch gone.
        _ => Ok(false),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedEpochState {
    Present,
    Replaced(String),
    MissingDetail,
    Absent,
}

fn classify_owned_epoch(
    receipt: &ManagedProcessCredential,
    census: &ProcessTopologyCensus,
) -> Result<OwnedEpochState> {
    if let Some(topology) = census.topology.get(&receipt.pid) {
        if topology.birth_identity.is_empty() {
            return Ok(OwnedEpochState::MissingDetail);
        }
        return Ok(if receipt_epoch_matches_topology(receipt, topology) {
            OwnedEpochState::Present
        } else {
            OwnedEpochState::Replaced(topology.birth_identity.clone())
        });
    }
    if let Some(zombie) = census.opaque_zombies.get(&receipt.pid) {
        let birth = zombie
            .birth_identity
            .as_ref()
            .filter(|birth| !birth.is_empty())
            .context("owned opaque zombie lacks birth identity")?;
        return Ok(if receipt_epoch_matches_opaque_zombie(receipt, zombie) {
            OwnedEpochState::Present
        } else {
            OwnedEpochState::Replaced(birth.clone())
        });
    }
    if census.listed_pids.contains(&receipt.pid) {
        Ok(OwnedEpochState::MissingDetail)
    } else {
        Ok(OwnedEpochState::Absent)
    }
}

fn owned_epoch_retired_by_stable_pair(
    receipt: &ManagedProcessCredential,
    before: &ProcessTopologyCensus,
    after: &ProcessTopologyCensus,
) -> Result<bool> {
    let first = classify_owned_epoch(receipt, before)?;
    let second = classify_owned_epoch(receipt, after)?;
    match (&first, &second) {
        (OwnedEpochState::Absent, OwnedEpochState::Absent) => Ok(true),
        (OwnedEpochState::Replaced(first), OwnedEpochState::Replaced(second))
            if first == second =>
        {
            Ok(true)
        }
        // Same-birth authority drift, declared-without-detail, and every A/B
        // transition preserve the old non-signal custody obligation.
        _ => Ok(false),
    }
}

fn current_owned_epoch_group(
    receipt: &ManagedProcessCredential,
    census: &ProcessTopologyCensus,
    provider_pgid: u32,
) -> Result<Option<u32>> {
    if let Some(topology) = census.topology.get(&receipt.pid) {
        if !receipt_epoch_matches_topology(receipt, topology) {
            bail!(
                "owned helper pid {} has a different birth and awaits stable A/B retirement",
                receipt.pid
            );
        }
        return Ok((topology.pgid != provider_pgid).then_some(topology.pgid));
    }
    if let Some(zombie) = census.opaque_zombies.get(&receipt.pid) {
        if !receipt_epoch_matches_opaque_zombie(receipt, zombie) {
            bail!(
                "owned helper pid {} has a different opaque birth and awaits stable A/B retirement",
                receipt.pid
            );
        }
        return Ok((zombie.pgid != provider_pgid).then_some(zombie.pgid));
    }
    if census.listed_pids.contains(&receipt.pid) {
        bail!(
            "owned helper pid {} is declared without exact detail",
            receipt.pid
        );
    }
    bail!(
        "owned helper pid {} is absent from one census and awaits stable A/B retirement",
        receipt.pid
    )
}

fn sidless_zombie_containment_proof(
    topology: &ManagedProcessTopology,
) -> Option<OpaqueZombieProof> {
    (topology.zombie
        && topology.sid == 0
        && topology.pgid != 0
        && !topology.birth_identity.is_empty())
    .then(|| OpaqueZombieProof {
        pid: topology.pid,
        observed_ppid: topology.observed_ppid,
        pgid: topology.pgid,
        uid: topology.uid,
        birth_identity: Some(topology.birth_identity.clone()),
    })
}

fn opaque_zombie_matches_receipt(
    zombie: &OpaqueZombieProof,
    receipt: &ManagedProcessCredential,
) -> bool {
    zombie.pid == receipt.pid
        && zombie.uid == receipt.uid
        && zombie.pgid == receipt.pgid
        && zombie
            .birth_identity
            .as_ref()
            .is_some_and(|birth| birth == &receipt.birth_identity)
}

/// Recover only the immutable epoch of an isolated direct provider that died
/// before a live executable receipt could settle. The Child remains unreaped,
/// so an exact kernel zombie row still binds PID/PPID/PGID/UID/birth. Because
/// setsid succeeded in the spawn hook, PID=PGID also fixes the original SID.
/// The sentinel executable can authorize zombie/session containment only; all
/// live full-credential paths must reject it.
fn capture_isolated_zombie_provider_epoch(
    provider_pid: u32,
) -> Result<Option<ManagedProcessCredential>> {
    // A Darwin fallback can strongly prove that the exact provider epoch is
    // still live even while PROC_PIDTBSDINFO is transiently unavailable. In
    // this one zombie-epoch capture context that means "not a zombie yet", so
    // the caller must continue to the ordinary full-credential settle path.
    // Keep inspect_opaque_zombie loud for all generic callers: LiveUnknown is
    // never absence and supplies neither zombie nor signal authority.
    #[cfg(target_os = "macos")]
    let zombie = match inspect_darwin_fallback_process(provider_pid)? {
        Some(DarwinFallbackProcess::Zombie(zombie)) => Some(zombie),
        Some(DarwinFallbackProcess::LiveUnknown(_)) | None => None,
    };
    #[cfg(not(target_os = "macos"))]
    let zombie = inspect_opaque_zombie(provider_pid)?;
    let Some(zombie) = zombie else {
        return Ok(None);
    };
    let Some(birth_identity) = zombie.birth_identity else {
        return Ok(None);
    };
    if zombie.pid != provider_pid
        || zombie.observed_ppid != std::process::id()
        || zombie.pgid != provider_pid
    {
        return Ok(None);
    }
    Ok(Some(ManagedProcessCredential {
        pid: provider_pid,
        observed_ppid: zombie.observed_ppid,
        pgid: provider_pid,
        sid: provider_pid,
        uid: zombie.uid,
        birth_identity,
        executable_summary: ZOMBIE_PROVIDER_EPOCH_EXECUTABLE.to_string(),
    }))
}

#[doc(hidden)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WakeSupervisorLaunchSpec {
    pub version: u32,
    #[serde(default = "default_wake_supervisor_protocol_revision")]
    protocol_revision: u32,
    pub token: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Schema-3 channel authority copied from the parent's immutable render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel_binding: Option<ChannelSupervisorBindingV1>,
    /// Whether `env` is the complete captured provider environment. Production
    /// bootstrap launches set this so the child can use `env_clear`; historical
    /// fixtures that carry only overrides retain inherited-environment semantics.
    #[serde(default, skip_serializing_if = "false_value")]
    pub environment_is_complete: bool,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
    pub ack_path: PathBuf,
    pub status_path: PathBuf,
    pub policy: WakeSupervisorPolicy,
    #[serde(default)]
    pub wake_id: Option<String>,
    /// Runtime registry identity. Legacy revision-2 and already-launched
    /// revision-3 documents may omit it; their terminal ledger fact is then
    /// explicitly classified as OperationalError rather than inferred.
    #[serde(default)]
    pub agent: Option<String>,
    /// Present only for a B192 continuation. These values cross the ACK
    /// boundary in the typed launch document and are copied into the immutable
    /// control descriptor before the provider can be adopted by a retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attach_action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continued_from_wake_id: Option<String>,
    #[serde(default = "ordinary_managed_wake_runtime_limit")]
    pub runtime_limit: ManagedWakeRuntimeLimit,
    #[serde(skip, default = "legacy_wake_supervisor_launch_variant")]
    pub launch_variant: WakeSupervisorLaunchVariant,
    /// Legacy A0001 field retained for typed-spec compatibility. New launchers
    /// leave it empty and use the light topology snapshot below.
    #[serde(default)]
    pub baseline: Vec<ManagedProcessCredential>,
    #[serde(default)]
    pub baseline_topology: Vec<ManagedProcessTopology>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChannelSupervisorBindingV1 {
    #[serde(default, skip_serializing_if = "false_value")]
    standalone: bool,
    // Optional only for decoding pre-B330 launch documents. New channel
    // launches supply both immutable tuples; they never contain environment secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_tuple: Option<super::InvocationTuple>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effective_tuple: Option<super::InvocationTuple>,
    alias: String,
    driver: crate::harness::HarnessId,
    action: String,
    config_digest: String,
    request_digest: String,
    attachment_manifest_digest: String,
    command_digest: String,
    program_identity_digest: String,
    configured_executable: PathBuf,
    configured_executable_identity_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrapper_executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrapper_identity_digest: Option<String>,
    head_root: PathBuf,
    fixed_head: String,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSupervisorLaunchVariant {
    LegacyRevision2,
    ManagedRevision3,
}

const LEGACY_WAKE_SUPERVISOR_PROTOCOL_REVISION: u32 = 2;
const WAKE_SUPERVISOR_PROTOCOL_REVISION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WakeSupervisorAck {
    token: String,
    phase: String,
    protocol_revision: u32,
    supervisor_pid: u32,
    provider_pid: u32,
    pgid: u32,
    provider_credential: ManagedProcessCredential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WakeSupervisorStatus {
    protocol_revision: u32,
    token: String,
    wake_id: String,
    #[serde(default)]
    agent: Option<String>,
    runtime_limit: ManagedWakeRuntimeLimit,
    completion_reason: ManagedWakeStopReason,
    hard_deadline_reached: bool,
    cancel_request_id: Option<String>,
    cancel_reason: Option<String>,
    terminal_seen: bool,
    exited_naturally: bool,
    signals: Vec<String>,
    containment_capability: ManagedContainmentCapability,
    containment_claim: ManagedContainmentClaim,
    managed_scope_terminated: bool,
    fork_complete: bool,
    process_tree_terminated: bool,
    owned_helpers: usize,
    termination_order: Vec<u32>,
    ambiguity: Vec<String>,
    exit_status: Option<i32>,
    error: Option<String>,
    #[serde(default, skip_serializing_if = "ManagedObservationDiagnostics::is_empty")]
    observation_diagnostics: ManagedObservationDiagnostics,
    topology_snapshots: usize,
    full_credential_inspections: usize,
    log_bytes_read: u64,
    #[serde(default)]
    last_frame_age_secs: Option<u64>,
    #[serde(default)]
    last_frame_summary: Option<String>,
    #[serde(default)]
    elapsed_secs: Option<u64>,
}

const MANAGED_WAKE_CONTROL_VERSION: u32 = 1;
const MANAGED_WAKE_DESCRIPTOR_VERSION: u32 = 2;
const MAX_MANAGED_WAKE_CONTROL_BYTES: usize = 64 * 1024;
const MAX_MANAGED_WAKE_CANCEL_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedWakeControlDescriptor {
    /// Immutable nonsecret channel facts. Version one descriptors predate this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invocation: Option<ManagedInvocationFactsV1>,
    version: u32,
    protocol_revision: u32,
    wake_id: String,
    token: String,
    supervisor_pid: u32,
    provider_pid: u32,
    stop_path: PathBuf,
    status_path: PathBuf,
    cancel_path: PathBuf,
    cancel_ack_path: PathBuf,
    runtime_limit: ManagedWakeRuntimeLimit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attach_action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    continued_from_wake_id: Option<String>,
}

/// Existing control authority augmented with invocation facts, not another journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedInvocationFactsV1 {
    binding: ChannelSupervisorBindingV1,
    cwd: PathBuf,
    log_path: PathBuf,
    published_at: String,
}

fn invocation_facts_from_launch(spec: &WakeSupervisorLaunchSpec) -> Option<ManagedInvocationFactsV1> {
    let binding = spec.channel_binding.as_ref()?;
    binding.requested_tuple.as_ref()?;
    binding.effective_tuple.as_ref()?;
    Some(ManagedInvocationFactsV1 {
        binding: binding.clone(),
        cwd: spec.cwd.clone(),
        log_path: spec.log_path.clone(),
        published_at: crate::util::now_rfc3339(),
    })
}

fn validate_invocation_facts(root: &Path, facts: &ManagedInvocationFactsV1) -> Result<()> {
    super::validate_alias(&facts.binding.alias)?;
    channel_binding_driver_contract(&facts.binding)?;
    if facts.binding.requested_tuple.is_none() || facts.binding.effective_tuple.is_none() {
        bail!("managed invocation lacks immutable requested/effective tuples");
    }
    for value in [&facts.binding.config_digest, &facts.binding.request_digest,
        &facts.binding.attachment_manifest_digest, &facts.binding.command_digest,
        &facts.binding.program_identity_digest, &facts.binding.configured_executable_identity_digest] {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            bail!("managed invocation digest is not canonical SHA-256");
        }
    }
    if facts.binding.fixed_head.len() != 40
        || !facts.binding.fixed_head.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("managed invocation fixed HEAD is not canonical");
    }
    for path in [&facts.cwd, &facts.binding.head_root, &facts.log_path] {
        if !path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
            bail!("managed invocation path is not absolute and normalized");
        }
    }
    if facts.binding.standalone && (facts.cwd != root || facts.binding.head_root != root || facts.binding.action != "consult") {
        bail!("standalone invocation scope does not match its project/action");
    }
    if facts.log_path.parent() != Some(root.join("coordination/runtime/logs").as_path())
        || humantime::parse_rfc3339(&facts.published_at).is_err() {
        bail!("managed invocation log/time binding is invalid");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ManagedWakeAttachDescriptorBinding<'a> {
    action_id: &'a str,
    source_wake_id: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedWakeCancelAck {
    version: u32,
    protocol_revision: u32,
    wake_id: String,
    token: String,
    request_id: String,
    state: String,
    winner: ManagedWakeStopReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedWakeStopReceipt {
    version: u32,
    protocol_revision: u32,
    wake_id: String,
    token: String,
    completion_reason: ManagedWakeStopReason,
    selected_at_monotonic_ms: u64,
    cancel_request_id: Option<String>,
    cancel_reason: Option<String>,
}

const MANAGED_SESSION_RECEIPT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenCodeSessionReceipt {
    version: u32,
    wake_id: String,
    token: String,
    agent: String,
    program_summary: String,
    session_id: String,
    session_digest: String,
    observed_at: String,
}

struct ManagedSessionReceiptSink {
    dir: PathBuf,
    path: PathBuf,
    wake_id: String,
    token: String,
    agent: String,
    program_summary: String,
    accepted: Option<String>,
}

impl ManagedSessionReceiptSink {
    fn new(dir: &Path, wake_id: &str, token: &str, agent: &str, program: &str) -> Result<Self> {
        let program_summary = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .context("managed session receipt program basename is not UTF-8")?
            .to_string();
        if program_summary != "opencode" {
            bail!("managed provider session receipts are supported only for opencode");
        }
        let path = dir.join(format!("{token}.session.json"));
        if path.exists() {
            bail!("managed provider session receipt path is not fresh");
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            path,
            wake_id: wake_id.to_string(),
            token: token.to_string(),
            agent: agent.to_string(),
            program_summary,
            accepted: None,
        })
    }

    fn observe_complete_line(&mut self, line: &str) -> Result<()> {
        let Some(session_id) = classify_opencode_session_receipt(line) else {
            return Ok(());
        };
        if let Some(existing) = self.accepted.as_deref() {
            if existing == session_id {
                return Ok(());
            }
            bail!(
                "managed provider emitted conflicting session receipts (digests {} and {})",
                sha256_hex(existing.as_bytes()),
                sha256_hex(session_id.as_bytes())
            );
        }
        let receipt = OpenCodeSessionReceipt {
            version: MANAGED_SESSION_RECEIPT_VERSION,
            wake_id: self.wake_id.clone(),
            token: self.token.clone(),
            agent: self.agent.clone(),
            program_summary: self.program_summary.clone(),
            session_digest: sha256_hex(session_id.as_bytes()),
            session_id: session_id.clone(),
            observed_at: humantime::format_rfc3339_seconds(std::time::SystemTime::now())
                .to_string(),
        };
        match atomic_create_json(&self.path, &receipt) {
            Ok(()) => {
                self.accepted = Some(session_id);
                Ok(())
            }
            Err(install_error) if self.path.exists() => {
                let existing = read_opencode_session_receipt_file(
                    &self.path,
                    &self.dir,
                    &self.wake_id,
                    &self.token,
                    &self.agent,
                )
                .with_context(|| {
                    format!(
                        "validate racing managed session receipt failed after atomic no-replace: {install_error:#}"
                    )
                })?;
                if existing.session_id != session_id {
                    bail!("racing managed provider session receipts conflict by digest");
                }
                self.accepted = Some(session_id);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

fn read_opencode_session_receipt_file(
    path: &Path,
    dir: &Path,
    expected_wake_id: &str,
    expected_token: &str,
    expected_agent: &str,
) -> Result<OpenCodeSessionReceipt> {
    let body = secure_read_lf_frame(path, dir, MAX_MANAGED_WAKE_CONTROL_BYTES)
        .context("read managed provider session receipt failed")?;
    let receipt: OpenCodeSessionReceipt =
        serde_json::from_slice(&body).context("parse managed provider session receipt failed")?;
    if receipt.version != MANAGED_SESSION_RECEIPT_VERSION
        || receipt.wake_id != expected_wake_id
        || receipt.token != expected_token
        || receipt.agent != expected_agent
        || receipt.program_summary != "opencode"
        || classify_opencode_session_receipt(
            &serde_json::json!({
                "type": "step_start",
                "sessionID": receipt.session_id,
            })
            .to_string(),
        )
        .as_deref()
            != Some(receipt.session_id.as_str())
        || receipt.session_digest != sha256_hex(receipt.session_id.as_bytes())
        || receipt.observed_at.trim().is_empty()
        || humantime::parse_rfc3339(&receipt.observed_at).is_err()
    {
        bail!("managed provider session receipt identity/digest mismatch");
    }
    Ok(receipt)
}

#[cfg(target_os = "macos")]
const OPEN_NOFOLLOW: i32 = 0x0000_0100;
#[cfg(any(target_os = "linux", target_os = "android"))]
const OPEN_NOFOLLOW: i32 = 0x0002_0000;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
const OPEN_NOFOLLOW: i32 = 0;

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and does not mutate memory.
    unsafe { getuid() }
}

fn authenticated_supervisor_dir(root: &Path, create: bool) -> Result<PathBuf> {
    let canonical_root = fs::canonicalize(root).context("canonicalize repository root failed")?;
    let coordination = canonical_root.join("coordination");
    let runtime = coordination.join("runtime");
    for component in [&coordination, &runtime] {
        let metadata = fs::symlink_metadata(component).with_context(|| {
            format!(
                "stat managed wake control parent failed: {}",
                component.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("managed wake control parent must be a real directory");
        }
    }
    let supervisors = runtime.join("supervisors");
    if !supervisors.exists() {
        if !create {
            bail!("unsupported-or-not-managed: supervisor directory is absent");
        }
        fs::create_dir(&supervisors).with_context(|| {
            format!(
                "create managed wake supervisor directory failed: {}",
                supervisors.display()
            )
        })?;
        fs::set_permissions(&supervisors, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(&supervisors)
        .context("stat managed wake supervisor directory failed")?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o022 != 0
    {
        bail!("managed wake supervisor directory owner/type/mode is untrusted");
    }
    if metadata.mode() & 0o777 != 0o700 {
        if !create {
            bail!("managed wake supervisor directory must retain mode 0700");
        }
        fs::set_permissions(&supervisors, fs::Permissions::from_mode(0o700))?;
    }
    let verified = fs::symlink_metadata(&supervisors)?;
    if verified.uid() != current_uid()
        || verified.mode() & 0o777 != 0o700
        || !verified.file_type().is_dir()
        || verified.file_type().is_symlink()
    {
        bail!("managed wake supervisor directory failed 0700 verification");
    }
    let canonical = fs::canonicalize(&supervisors)?;
    if canonical != supervisors || !canonical.starts_with(&canonical_root) {
        bail!("managed wake supervisor directory escaped repository root");
    }
    Ok(canonical)
}

fn secure_read_lf_frame(path: &Path, expected_dir: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    if path.parent() != Some(expected_dir) {
        bail!("managed wake sidecar path escaped authenticated directory");
    }
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("stat managed wake sidecar failed: {}", path.display()))?;
    if before.file_type().is_symlink()
        || !before.file_type().is_file()
        || before.uid() != current_uid()
        || before.mode() & 0o777 != 0o600
        || before.len() as usize > max_bytes
    {
        bail!("managed wake sidecar type/owner/mode/size is invalid");
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open managed wake sidecar failed: {}", path.display()))?;
    let opened = file.metadata()?;
    if opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || opened.uid() != current_uid()
        || opened.mode() & 0o777 != 0o600
        || !opened.file_type().is_file()
    {
        bail!("managed wake sidecar inode changed during no-follow open");
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes
        || !bytes.ends_with(b"\n")
        || bytes.len() < 2
        || bytes[..bytes.len() - 1]
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r'))
    {
        bail!("managed wake sidecar must contain one bounded LF-framed JSON document");
    }
    bytes.pop();
    Ok(bytes)
}

struct ManagedWakeControlRuntime {
    dir: PathBuf,
    wake_id: String,
    token: String,
    cancel_path: PathBuf,
    cancel_ack_path: PathBuf,
    stop_path: PathBuf,
    provider_started_at: Instant,
    cancel_request: Option<ManagedWakeCancelRequest>,
}

// Control artifacts use a smaller, independently enforced representation bound
// than launch documents. Check it before spawn as well as before publication.
fn build_control_descriptor(
    root: &Path,
    wake_id: &str,
    token: &str,
    supervisor_pid: u32,
    provider_pid: u32,
    status_path: &Path,
    runtime_limit: ManagedWakeRuntimeLimit,
    attach_binding: Option<ManagedWakeAttachDescriptorBinding<'_>>,
    invocation: Option<ManagedInvocationFactsV1>,
) -> Result<ManagedWakeControlDescriptor> {
    if let Some(facts) = invocation.as_ref() { validate_invocation_facts(root, facts)?; }
    let dir = root.join("coordination/runtime/supervisors");
    let descriptor = ManagedWakeControlDescriptor {
            version: if invocation.is_some() { MANAGED_WAKE_DESCRIPTOR_VERSION } else { MANAGED_WAKE_CONTROL_VERSION },
            invocation,
            protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
            wake_id: wake_id.to_string(),
            token: token.to_string(),
            supervisor_pid,
            provider_pid,
            stop_path: dir.join(format!("{token}.stop.json")),
            status_path: status_path.to_path_buf(),
            cancel_path: dir.join(format!("{token}.cancel.json")),
            cancel_ack_path: dir.join(format!("{token}.cancel-ack.json")),
            runtime_limit,
            attach_action_id: attach_binding.map(|binding| binding.action_id.to_string()),
            continued_from_wake_id: attach_binding
                .map(|binding| binding.source_wake_id.to_string()),
        };
    if serde_json::to_vec(&descriptor)?.len() + 1 > MAX_MANAGED_WAKE_CONTROL_BYTES {
        bail!("local managed control descriptor exceeds its 64 KiB representation bound before spawn; this is not a provider/model capability limit");
    }
    Ok(descriptor)
}

impl ManagedWakeControlRuntime {
    fn publish(
        root: &Path,
        wake_id: &str,
        token: &str,
        supervisor_pid: u32,
        provider_pid: u32,
        status_path: &Path,
        runtime_limit: ManagedWakeRuntimeLimit,
        provider_started_at: Instant,
        attach_binding: Option<ManagedWakeAttachDescriptorBinding<'_>>,
        invocation: Option<ManagedInvocationFactsV1>,
    ) -> Result<Self> {
        if !is_strict_uuid(wake_id) || !is_lower_hex_token(token) {
            bail!("managed wake control identity is malformed");
        }
        let dir = authenticated_supervisor_dir(root, true)?;
        let control_path = dir.join(format!("{wake_id}.control.json"));
        let cancel_path = dir.join(format!("{token}.cancel.json"));
        let cancel_ack_path = dir.join(format!("{token}.cancel-ack.json"));
        let stop_path = dir.join(format!("{token}.stop.json"));
        let expected_status = dir.join(format!("{token}.status.json"));
        if status_path != expected_status {
            bail!("managed wake status path does not match authenticated token path");
        }
        for path in [&control_path, &cancel_path, &cancel_ack_path, &stop_path] {
            if path.exists() {
                bail!("managed wake control path is not fresh: {}", path.display());
            }
        }
        let descriptor = build_control_descriptor(root, wake_id, token, supervisor_pid,
            provider_pid, status_path, runtime_limit, attach_binding, invocation)?;
        atomic_create_json(&control_path, &descriptor)?;
        Ok(Self {
            dir,
            wake_id: wake_id.to_string(),
            token: token.to_string(),
            cancel_path,
            cancel_ack_path,
            stop_path,
            provider_started_at,
            cancel_request: None,
        })
    }

    fn poll_authenticated_cancel(
        &mut self,
        current_winner: Option<ManagedWakeStopReason>,
    ) -> Result<bool> {
        if self.cancel_request.is_some() {
            return Ok(true);
        }
        if !self.cancel_path.exists() {
            return Ok(false);
        }
        let body =
            secure_read_lf_frame(&self.cancel_path, &self.dir, MAX_MANAGED_WAKE_CANCEL_BYTES)?;
        let request = parse_managed_wake_cancel_request(&body, &self.wake_id, &self.token)
            .map_err(anyhow::Error::msg)?;
        let winner = current_winner.unwrap_or(ManagedWakeStopReason::ManualCancel);
        let ack = ManagedWakeCancelAck {
            version: MANAGED_WAKE_CONTROL_VERSION,
            protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
            wake_id: self.wake_id.clone(),
            token: self.token.clone(),
            request_id: request.request_id.clone(),
            state: if current_winner.is_some() {
                "already-terminal".to_string()
            } else {
                "accepted".to_string()
            },
            winner,
        };
        atomic_create_json(&self.cancel_ack_path, &ack)?;
        self.cancel_request = Some(request);
        Ok(true)
    }

    fn publish_stop(&self, winner: ManagedWakeStopReason) -> Result<()> {
        let receipt = ManagedWakeStopReceipt {
            version: MANAGED_WAKE_CONTROL_VERSION,
            protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
            wake_id: self.wake_id.clone(),
            token: self.token.clone(),
            completion_reason: winner,
            selected_at_monotonic_ms: self
                .provider_started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            cancel_request_id: self
                .cancel_request
                .as_ref()
                .map(|request| request.request_id.clone()),
            cancel_reason: self
                .cancel_request
                .as_ref()
                .map(|request| request.reason.clone()),
        };
        match atomic_create_json(&self.stop_path, &receipt) {
            Ok(()) => Ok(()),
            Err(install_error) if self.stop_path.exists() => {
                let body = secure_read_lf_frame(
                    &self.stop_path,
                    &self.dir,
                    MAX_MANAGED_WAKE_CONTROL_BYTES,
                )?;
                let installed: ManagedWakeStopReceipt = serde_json::from_slice(&body)
                    .context("parse existing managed wake stop receipt failed")?;
                if installed == receipt {
                    Ok(())
                } else {
                    bail!("managed wake stop receipt conflicts with immutable winner: {install_error:#}")
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn path_is_regular_nonsymlink(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat path failed: {}", path.display()))?;
    Ok(metadata.file_type().is_file() && !metadata.file_type().is_symlink())
}

fn canonical_parent(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().context("path has no parent")?;
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("stat raw parent failed: {}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("path parent must be a real directory, not a symlink");
    }
    fs::canonicalize(parent)
        .with_context(|| format!("canonicalize parent failed: {}", parent.display()))
}

fn channel_binding_driver_contract(
    binding: &ChannelSupervisorBindingV1,
) -> Result<crate::harness::DriverContract> {
    let action = match binding.action.as_str() {
        "execute" => crate::harness::DriverAction::Execute,
        "review" => crate::harness::DriverAction::Review,
        "consult" => crate::harness::DriverAction::Consult,
        _ => bail!("channel supervisor action 未建模"),
    };
    binding
        .driver
        .driver_contract(action)
        .context("channel supervisor driver/action 不支持")
}

fn validate_channel_dsh_environment(
    driver: crate::harness::HarnessId,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if driver == crate::harness::HarnessId::Dsh && env.contains_key("ORCH_DSH_ZSTD_BIN") {
        bail!("channel DSH launch spec 禁止未建模的 ORCH_DSH_ZSTD_BIN override");
    }
    Ok(())
}

fn validate_channel_supervisor_live_binding(
    root: &Path,
    spec: &WakeSupervisorLaunchSpec,
) -> Result<()> {
    let Some(binding) = spec.channel_binding.as_ref() else {
        return Ok(());
    };

    if binding.standalone && crate::has_selfhost_state(root)? {
        bail!("standalone invocation cannot bypass newly present selfhost state");
    }
    let contract = channel_binding_driver_contract(binding)?;
    validate_channel_dsh_environment(binding.driver, &spec.env)?;
    let canonical_root = fs::canonicalize(root)?;
    if crate::channel::executable_identity_digest_v1(&binding.configured_executable)?
        != binding.configured_executable_identity_digest
    {
        bail!("channel configured executable identity 漂移 before provider spawn");
    }
    let program = Path::new(
        spec.argv
            .first()
            .context("channel supervisor argv 缺 program")?,
    );
    if crate::channel::executable_identity_digest_v1(program)? != binding.program_identity_digest {
        bail!("channel program identity 漂移 before provider spawn");
    }
    match (
        contract.wrapper,
        binding.wrapper_executable.as_ref(),
        binding.wrapper_identity_digest.as_deref(),
    ) {
        (Some(relative), Some(wrapper), Some(expected)) => {
            let expected_wrapper = canonical_root.join(relative);
            if wrapper != &expected_wrapper {
                bail!("channel wrapper path 与 driver contract 不一致");
            }
            if crate::channel::code_owned_wrapper_identity_digest_v1(wrapper)? != expected {
                bail!("channel wrapper identity 漂移 before provider spawn");
            }
            let rendered_wrapper = spec
                .argv
                .get(1)
                .map(Path::new)
                .context("channel wrapper invocation 缺 argv[1]")?;
            if program != Path::new("/bin/sh") || rendered_wrapper != wrapper {
                bail!("channel wrapper argv 与 immutable binding 不一致");
            }
            match contract.transport {
                crate::harness::HarnessTransport::CliStream => {
                    if spec.env.get("ORCH_HARNESS_PROVIDER_BIN").map(Path::new)
                        != Some(binding.configured_executable.as_path())
                    {
                        bail!("channel stream provider executable envelope 与 binding 不一致");
                    }
                }
                crate::harness::HarnessTransport::SocketInject
                | crate::harness::HarnessTransport::HttpInject => {
                    if spec.env.contains_key("ORCH_HARNESS_PROVIDER_BIN") {
                        bail!("channel injection wrapper 禁止 provider executable envelope");
                    }
                }
                _ => bail!("channel wrapper transport 与 driver contract 不一致"),
            }
        }
        (None, None, None) => {
            if contract.transport != crate::harness::HarnessTransport::CliDirect {
                bail!("channel direct transport 与 driver contract 不一致");
            }
            if fs::canonicalize(program)? != fs::canonicalize(&binding.configured_executable)? {
                bail!("channel direct program 与 configured executable 不一致");
            }
        }
        _ => bail!("channel wrapper identity 与 driver contract 不一致"),
    }

    let head_metadata = fs::symlink_metadata(&binding.head_root).with_context(|| {
        format!(
            "stat channel fixed-HEAD root 失败: {}",
            binding.head_root.display()
        )
    })?;
    let canonical_head_root = fs::canonicalize(&binding.head_root)?;
    if head_metadata.file_type().is_symlink()
        || !head_metadata.file_type().is_dir()
        || canonical_head_root != binding.head_root
    {
        bail!("channel fixed-HEAD root 必须是 exact canonical no-symlink directory");
    }
    if crate::gitx::canonical_worktree_common_dir(&canonical_root)?
        != crate::gitx::canonical_worktree_common_dir(&canonical_head_root)?
    {
        bail!("channel fixed-HEAD root 属于不同 git common-dir");
    }
    let actual_head = crate::gitx::rev_parse(&canonical_head_root, "HEAD^{commit}")?;
    if actual_head != binding.fixed_head {
        bail!(
            "channel fixed HEAD 漂移 before provider spawn: expected={} actual={actual_head}",
            binding.fixed_head
        );
    }
    Ok(())
}

/// Parse and fully validate the one-shot launch document. Diagnostics mention
/// only scoped paths and field names; provider argv is intentionally omitted.
#[doc(hidden)]
pub fn parse_wake_supervisor_launch_spec(
    root: &Path,
    bytes: &[u8],
) -> std::result::Result<WakeSupervisorLaunchSpec, String> {
    fn parse(root: &Path, bytes: &[u8]) -> Result<WakeSupervisorLaunchSpec> {
        if bytes.len() > 1024 * 1024 {
            bail!("wake supervisor launch spec exceeds 1 MiB");
        }
        let raw: serde_json::Value = serde_json::from_slice(bytes)
            .context("wake supervisor launch spec is not valid typed JSON")?;
        let has_wake_id = raw.get("wakeId").is_some();
        let has_runtime_limit = raw.get("runtimeLimit").is_some();
        let mut spec: WakeSupervisorLaunchSpec = serde_json::from_value(raw)
            .context("wake supervisor launch spec is not valid typed JSON")?;
        if spec.version != 1 {
            bail!("wake supervisor launch spec version must be 1");
        }
        match spec.protocol_revision {
            LEGACY_WAKE_SUPERVISOR_PROTOCOL_REVISION if !has_wake_id && !has_runtime_limit => {
                spec.launch_variant = WakeSupervisorLaunchVariant::LegacyRevision2;
                spec.runtime_limit = managed_wake_runtime_limit(None);
            }
            WAKE_SUPERVISOR_PROTOCOL_REVISION if has_wake_id && has_runtime_limit => {
                let wake_id = spec
                    .wake_id
                    .as_deref()
                    .context("wake supervisor launch spec wakeId is required")?;
                if !is_strict_uuid(wake_id) {
                    bail!("wake supervisor launch spec wakeId must be a strict UUID");
                }
                let recomputed =
                    managed_wake_runtime_limit(spec.runtime_limit.requested_review_deadline_secs);
                if spec.runtime_limit != recomputed {
                    bail!("wake supervisor launch spec runtimeLimit is not canonical");
                }
                spec.launch_variant = WakeSupervisorLaunchVariant::ManagedRevision3;
            }
            LEGACY_WAKE_SUPERVISOR_PROTOCOL_REVISION => {
                bail!("legacy wake supervisor launch spec must omit wakeId/runtimeLimit")
            }
            WAKE_SUPERVISOR_PROTOCOL_REVISION => {
                bail!("revision 3 wake supervisor launch spec requires wakeId/runtimeLimit")
            }
            _ => bail!("wake supervisor launch spec protocolRevision must be 2 (legacy) or 3"),
        }
        if let Some(agent) = spec.agent.as_deref() {
            validate_agent_component(agent)
                .context("wake supervisor launch spec agent is invalid")?;
        }
        if let Some(binding) = spec.channel_binding.as_ref() {
            if spec.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
                || spec.agent.as_deref() != Some(binding.alias.as_str())
                || !spec.environment_is_complete
            {
                bail!("channel supervisor binding 要求 revision-3、exact alias 与 complete env");
            }
            validate_agent_component(&binding.alias).context("channel supervisor alias invalid")?;
            channel_binding_driver_contract(binding)?;
            for (label, digest) in [
                ("configDigest", binding.config_digest.as_str()),
                ("requestDigest", binding.request_digest.as_str()),
                (
                    "attachmentManifestDigest",
                    binding.attachment_manifest_digest.as_str(),
                ),
                ("commandDigest", binding.command_digest.as_str()),
                (
                    "programIdentityDigest",
                    binding.program_identity_digest.as_str(),
                ),
                (
                    "configuredExecutableIdentityDigest",
                    binding.configured_executable_identity_digest.as_str(),
                ),
            ] {
                if !valid_sha256(digest) {
                    bail!("channel supervisor {label} 不是 lowercase hex64");
                }
            }
            match (
                binding.wrapper_executable.as_ref(),
                binding.wrapper_identity_digest.as_deref(),
            ) {
                (None, None) => {}
                (Some(path), Some(digest)) if path.is_absolute() && valid_sha256(digest) => {}
                _ => bail!("channel supervisor wrapper identity binding 不完整或非法"),
            }
            if !binding.configured_executable.is_absolute()
                || !binding.head_root.is_absolute()
                || !is_full_review_head(&binding.fixed_head)
            {
                bail!("channel supervisor executable/head binding 不完整或非法");
            }
        }
        match (
            spec.attach_action_id.as_deref(),
            spec.continued_from_wake_id.as_deref(),
        ) {
            (None, None) => {}
            (Some(action_id), Some(source_wake_id))
                if spec.protocol_revision == WAKE_SUPERVISOR_PROTOCOL_REVISION
                    && action_id.len() == 64
                    && action_id
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && is_strict_uuid(source_wake_id)
                    && spec.wake_id.as_deref() != Some(source_wake_id) => {}
            _ => bail!("wake supervisor attach binding is incomplete or malformed"),
        }
        if spec.token.len() != 32
            || !spec
                .token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("wake supervisor token must be 32 lowercase hex characters");
        }
        if spec.argv.is_empty()
            || spec.argv.iter().any(|arg| arg.as_bytes().contains(&0))
            || spec.argv.iter().map(|arg| arg.len()).sum::<usize>() > 512 * 1024
        {
            bail!("wake supervisor argv is empty, contains NUL, or exceeds bounds");
        }
        let canonical_root = fs::canonicalize(root).context("canonicalize root failed")?;
        let program = Path::new(&spec.argv[0]);
        let canonical_program =
            fs::canonicalize(program).context("canonicalize wake supervisor program failed")?;
        let program_metadata =
            fs::symlink_metadata(program).context("stat raw wake supervisor program failed")?;
        // D-25 (r78 planner direct fix): custody admission is a *topology* question
        // ("is argv[1] one of the registered stream wrappers whose child process tree
        // the supervisor owns?"), not a *receipt-grammar* question ("what shape of
        // backend receipt does this provider emit?").  Using the receipt classifier
        // here silently excluded DSH — whose receipt capability was absent at
        // the time — so a real DSH review was leased and then refused
        // immediately before spawn (r77 event 01M0MJMYY94WCSZR0QXYC3B45D).
        let registered_wrapper = managed_stream_harness(&spec.argv).is_some_and(|_| {
            let wrapper = Path::new(&spec.argv[1]);
            let wrapper = if wrapper.is_absolute() {
                wrapper.to_path_buf()
            } else {
                canonical_root.join(wrapper)
            };
            let expected = canonical_root.join("orch/scripts").join(
                Path::new(&spec.argv[1])
                    .file_name()
                    .expect("managed wrapper classifier supplied a basename"),
            );
            fs::canonicalize(&wrapper).ok().as_deref() == Some(expected.as_path())
                && fs::symlink_metadata(&wrapper).ok().is_some_and(|metadata| {
                    metadata.file_type().is_file() && !metadata.file_type().is_symlink()
                })
        });
        let direct_provider = matches!(
            program.file_name().and_then(|name| name.to_str()),
            Some("codex" | "opencode" | "agy")
        );
        let wrapper_shell = registered_wrapper
            && matches!(
                program.file_name().and_then(|name| name.to_str()),
                Some("sh" | "bash")
            );
        let channel_program = if let Some(binding) = spec.channel_binding.as_ref() {
            crate::channel::executable_identity_digest_v1(program)?
                == binding.program_identity_digest
        } else {
            false
        };
        if !program.is_absolute()
            || !program_metadata.file_type().is_file()
            || program_metadata.file_type().is_symlink()
            || program_metadata.mode() & 0o111 == 0
            || (!direct_provider && !wrapper_shell && !channel_program)
        {
            bail!("wake supervisor executable program must be an authenticated absolute regular provider or code-owned wrapper shell");
        }
        if spec.channel_binding.is_none() {
            spec.argv[0] = canonical_program.to_string_lossy().into_owned();
        }
        let cwd_metadata = fs::symlink_metadata(&spec.cwd).context("stat raw cwd failed")?;
        let canonical_cwd = fs::canonicalize(&spec.cwd).context("canonicalize cwd failed")?;
        if cwd_metadata.file_type().is_symlink() || !cwd_metadata.file_type().is_dir() {
            bail!("wake supervisor cwd must be a regular no-symlink directory");
        }
        if spec.channel_binding.is_some() {
            if canonical_cwd != spec.cwd {
                bail!("channel wake supervisor cwd must use its exact canonical spelling");
            }
            if canonical_cwd != canonical_root {
                if !canonical_cwd.starts_with(&canonical_root) {
                    bail!("channel wake supervisor linked worktree must stay inside root");
                }
                validate_review_site_repository(&canonical_root, &canonical_cwd)
                    .context("channel wake supervisor cwd is not an exact linked worktree root")?;
            }
        } else if canonical_cwd != canonical_root {
            bail!("legacy wake supervisor cwd must equal canonical root");
        }
        spec.cwd = canonical_cwd;
        validate_channel_supervisor_live_binding(&canonical_root, &spec)?;
        let logs = canonical_root.join("coordination/runtime/logs");
        let canonical_logs = fs::canonicalize(&logs)?;
        if canonical_parent(&spec.log_path)? != canonical_logs
            || !path_is_regular_nonsymlink(&spec.log_path)?
        {
            bail!(
                "wake supervisor logPath must be an existing regular non-symlink in runtime logs"
            );
        }
        spec.log_path = fs::canonicalize(&spec.log_path)?;
        let supervisor_parent = authenticated_supervisor_dir(&canonical_root, true)
            .context("authenticate supervisor directory failed")?;
        for (path, suffix) in [
            (&spec.ack_path, "ack.json"),
            (&spec.status_path, "status.json"),
        ] {
            if path.exists()
                || canonical_parent(path)? != supervisor_parent
                || path.file_name().and_then(|name| name.to_str())
                    != Some(format!("{}.{}", spec.token, suffix).as_str())
            {
                bail!("wake supervisor ack/status target is not a fresh token-scoped path");
            }
        }
        spec.ack_path = supervisor_parent.join(format!("{}.ack.json", spec.token));
        spec.status_path = supervisor_parent.join(format!("{}.status.json", spec.token));
        if spec.policy != WakeSupervisorPolicy::production() {
            bail!("wake supervisor policy does not match production constants");
        }
        if !spec.baseline.is_empty() && !spec.baseline_topology.is_empty() {
            bail!("wake supervisor launch spec has conflicting baseline encodings");
        }
        if let Some(wake_id) = spec.wake_id.as_deref() {
            // Both real PIDs have at most this many decimal digits. Published
            // UTC timestamps use fixed second precision; no labels are capped.
            let attach = spec.attach_action_id.as_deref().zip(spec.continued_from_wake_id.as_deref())
                .map(|(action_id, source_wake_id)| ManagedWakeAttachDescriptorBinding { action_id, source_wake_id });
            build_control_descriptor(&canonical_root, wake_id, &spec.token, u32::MAX,
                u32::MAX, &spec.status_path, spec.runtime_limit, attach,
                invocation_facts_from_launch(&spec))?;
        }
        Ok(spec)
    }
    parse(root, bytes).map_err(|error| format!("{error:#}"))
}

fn executable_summary(path: &Path) -> Result<String> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize executable failed: {}", path.display()))?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("stat executable failed: {}", canonical.display()))?;
    let mut digest = Sha256::new();
    digest.update(canonical.as_os_str().as_encoded_bytes());
    digest.update(metadata.dev().to_le_bytes());
    digest.update(metadata.ino().to_le_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn inspect_stable_process(pid: u32) -> Result<Option<ManagedProcessCredential>> {
    let mut previous = None;
    for _ in 0..20 {
        let current = inspect_process(pid)?;
        if current.is_some() && current == previous {
            return Ok(current);
        }
        previous = current;
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn inspect_topology(pid: u32) -> Result<Option<ManagedProcessTopology>> {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let stat = match fs::read_to_string(proc_dir.join("stat")) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read /proc stat failed"),
    };
    let open = stat.find('(').context("malformed /proc stat command")?;
    let close = stat.rfind(')').context("malformed /proc stat")?;
    if close <= open {
        bail!("malformed /proc stat command bounds");
    }
    let executable_hint = stat[open + 1..close].to_string();
    let fields = stat[close + 1..].split_whitespace().collect::<Vec<_>>();
    if fields.len() <= 19 {
        bail!("short /proc stat");
    }
    let parse = |index: usize, name: &str| -> Result<u32> {
        fields[index]
            .parse::<u32>()
            .with_context(|| format!("parse /proc stat {name} failed"))
    };
    let status = match fs::read_to_string(proc_dir.join("status")) {
        Ok(status) => status,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read /proc status failed"),
    };
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|value| value.split_whitespace().next())
        .context("missing /proc status Uid")?
        .parse::<u32>()
        .context("parse /proc status Uid failed")?;
    Ok(Some(ManagedProcessTopology {
        pid,
        observed_ppid: parse(1, "ppid")?,
        pgid: parse(2, "pgrp")?,
        sid: parse(3, "session")?,
        uid,
        birth_identity: format!("linux-ticks:{}", fields[19]),
        zombie: fields[0] == "Z",
        executable_hint,
    }))
}

#[cfg(target_os = "linux")]
fn inspect_process(pid: u32) -> Result<Option<ManagedProcessCredential>> {
    let Some(topology) = inspect_topology(pid)? else {
        return Ok(None);
    };
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let exe = match fs::read_link(proc_dir.join("exe")) {
        Ok(exe) => exe,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read /proc exe failed"),
    };
    Ok(Some(ManagedProcessCredential {
        pid: topology.pid,
        observed_ppid: topology.observed_ppid,
        pgid: topology.pgid,
        sid: topology.sid,
        uid: topology.uid,
        birth_identity: topology.birth_identity,
        executable_summary: executable_summary(&exe)?,
    }))
}

#[cfg(target_os = "linux")]
fn process_topology_snapshot() -> Result<BTreeMap<u32, ManagedProcessTopology>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir("/proc").context("scan /proc failed")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(topology) = inspect_topology(pid)? {
            result.insert(pid, topology);
        }
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
fn inspect_opaque_zombie(pid: u32) -> Result<Option<OpaqueZombieProof>> {
    Ok(inspect_topology(pid)?.and_then(|topology| {
        topology.zombie.then_some(OpaqueZombieProof {
            pid: topology.pid,
            observed_ppid: topology.observed_ppid,
            pgid: topology.pgid,
            uid: topology.uid,
            birth_identity: Some(topology.birth_identity),
        })
    }))
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcBsdInfo {
    flags: u32,
    status: u32,
    xstatus: u32,
    pid: u32,
    ppid: u32,
    uid: u32,
    gid: u32,
    ruid: u32,
    rgid: u32,
    svuid: u32,
    svgid: u32,
    rfu_1: u32,
    comm: [u8; 16],
    name: [u8; 32],
    nfiles: u32,
    pgid: u32,
    pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    nice: i32,
    start_tvsec: u64,
    start_tvusec: u64,
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcBsdShortInfo {
    pid: u32,
    ppid: u32,
    pgid: u32,
    status: u32,
    comm: [u8; 16],
    flags: u32,
    uid: u32,
    gid: u32,
    ruid: u32,
    rgid: u32,
    svuid: u32,
    svgid: u32,
    rfu: u32,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DarwinShortProcessProof {
    pid: u32,
    observed_ppid: u32,
    pgid: u32,
    status: u32,
    uid: u32,
}

#[cfg(target_os = "macos")]
enum DarwinFallbackProcess {
    Zombie(OpaqueZombieProof),
    LiveUnknown(ManagedProcessTopology),
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DarwinKernelProcessKind {
    Live,
    Zombie,
}

#[cfg(target_os = "macos")]
fn classify_darwin_kernel_process_kind(status: u8) -> Result<DarwinKernelProcessKind> {
    match status {
        1..=4 => Ok(DarwinKernelProcessKind::Live),
        5 => Ok(DarwinKernelProcessKind::Zombie),
        _ => bail!("KERN_PROC_PID returned invalid process status {status}"),
    }
}

/// Minimal typed mirror of the public 64-bit Darwin `struct kinfo_proc` ABI.
/// Opaque byte ranges intentionally hide fields we never interpret; named
/// fields and const offsets prevent a same-size layout drift from silently
/// becoming signal authority.
#[cfg(target_os = "macos")]
#[repr(C)]
struct KinfoProcZombie {
    start_sec: i64,
    start_usec: i32,
    opaque_before_status: [u8; 24],
    status: u8,
    align_pid: [u8; 3],
    pid: i32,
    opaque_before_uid: [u8; 376],
    uid: u32,
    opaque_before_parent: [u8; 136],
    ppid: i32,
    pgid: i32,
    opaque_tail: [u8; 80],
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DarwinKernProcDisposition {
    Missing,
    Full,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DarwinFixedReadDisposition {
    Missing,
    Exact,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DarwinFullReadDisposition {
    Missing,
    PermissionDenied,
    Exact,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DarwinPidPathDisposition {
    Missing,
    Exact(usize),
    /// Zero length with `ENOENT` rather than `ESRCH`: Darwin can report this for
    /// a process that is still very much alive (r77 observed it while a live
    /// OpenCode reviewer was being walked across 458 helper processes).  It is
    /// therefore *not* a death signal and *not* an operational error on its own
    /// — the caller must re-read the topology and decide.  See D-26.
    EmptyWithoutEsrch,
}

#[cfg(target_os = "macos")]
enum DarwinFullTopologyRead {
    Missing,
    PermissionDenied,
    Exact(ManagedProcessTopology),
}

/// libproc fixed-structure calls use 0 for an unavailable/raced-away row.
/// Negative values and positive partial/oversized bodies are faults, never a
/// synonym for absence.
#[cfg(target_os = "macos")]
fn classify_darwin_fixed_read(
    label: &str,
    written: i32,
    raw_os_error: Option<i32>,
    expected: usize,
) -> Result<DarwinFixedReadDisposition> {
    if written < 0 {
        bail!("{label} failed with negative byte count {written}: errno={raw_os_error:?}");
    }
    if written == 0 {
        if raw_os_error == Some(3) {
            return Ok(DarwinFixedReadDisposition::Missing);
        }
        bail!("{label} returned zero bytes without ESRCH: errno={raw_os_error:?}");
    }
    if raw_os_error.unwrap_or_default() != 0 {
        bail!("{label} returned positive bytes with errno={raw_os_error:?}");
    }
    if written as usize != expected {
        bail!("{label} returned a positive non-exact body: expected={expected} actual={written}");
    }
    Ok(DarwinFixedReadDisposition::Exact)
}

/// SHORTBSDINFO is optional corroboration for the stronger birth-bearing
/// KERN_PROC_PID fallback. A declared PID may deny SHORT access or race out of
/// libproc while KERN still proves its exact live/zombie epoch. Only those
/// two zero-byte envelopes continue without a short proof; KERN remains loud
/// on every outcome other than an exact row or ESRCH disappearance.
#[cfg(target_os = "macos")]
fn classify_darwin_short_fallback_read(
    written: i32,
    raw_os_error: Option<i32>,
    expected: usize,
) -> Result<DarwinFixedReadDisposition> {
    if written == 0 && matches!(raw_os_error, Some(1 | 3 | 13)) {
        return Ok(DarwinFixedReadDisposition::Missing);
    }
    classify_darwin_fixed_read("PROC_PIDT_SHORTBSDINFO", written, raw_os_error, expected)
}

#[cfg(target_os = "macos")]
fn classify_darwin_full_read(
    written: i32,
    raw_os_error: Option<i32>,
    expected: usize,
) -> Result<DarwinFullReadDisposition> {
    if written == 0 && matches!(raw_os_error, Some(1 | 13)) {
        return Ok(DarwinFullReadDisposition::PermissionDenied);
    }
    Ok(
        match classify_darwin_fixed_read("PROC_PIDTBSDINFO", written, raw_os_error, expected)? {
            DarwinFixedReadDisposition::Missing => DarwinFullReadDisposition::Missing,
            DarwinFixedReadDisposition::Exact => DarwinFullReadDisposition::Exact,
        },
    )
}

#[cfg(target_os = "macos")]
fn checked_darwin_pid_t(label: &str, pid: u32) -> Result<i32> {
    if pid == 0 {
        bail!("{label} must be a positive Darwin pid_t");
    }
    i32::try_from(pid).with_context(|| format!("{label} exceeds Darwin pid_t"))
}

#[cfg(target_os = "macos")]
fn classify_darwin_pidpath_read(
    length: i32,
    raw_os_error: Option<i32>,
    buffer: &[u8],
) -> Result<DarwinPidPathDisposition> {
    if length < 0 {
        bail!("proc_pidpath returned negative length {length}: errno={raw_os_error:?}");
    }
    if length == 0 {
        if raw_os_error == Some(3) {
            return Ok(DarwinPidPathDisposition::Missing);
        }
        if raw_os_error == Some(2) {
            // D-26: ENOENT here is ambiguous, not terminal.  Treating it as an
            // operational error killed a working primary reviewer mid-review
            // (r77 wake 01a02924-4b7e-4962-8e3e-816576bb4406); treating it as
            // "alive" would let a dead process pass.  Hand the ambiguity back
            // to the caller, which resolves it against the process topology.
            return Ok(DarwinPidPathDisposition::EmptyWithoutEsrch);
        }
        bail!("proc_pidpath returned zero length without ESRCH: errno={raw_os_error:?}");
    }
    if raw_os_error.unwrap_or_default() != 0 {
        bail!("proc_pidpath returned a positive length with errno={raw_os_error:?}");
    }
    let length = usize::try_from(length).context("proc_pidpath length did not fit usize")?;
    if length >= buffer.len() {
        bail!(
            "proc_pidpath returned truncated/nonterminated length: length={length} capacity={}",
            buffer.len()
        );
    }
    if buffer[..length].contains(&0) {
        bail!("proc_pidpath returned an embedded NUL in its positive-length body");
    }
    if buffer[length] != 0 {
        bail!("proc_pidpath positive-length body lacks a trailing NUL");
    }
    Ok(DarwinPidPathDisposition::Exact(length))
}

/// What a `proc_pidpath` "zero length + ENOENT" means once the topology has been
/// re-read for the same pid (D-26).
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
enum DarwinPidPathRecheck {
    /// The process is gone, or the pid now belongs to a different epoch.  Either
    /// way the process we were asked about is absent — that is an ordinary
    /// observation, not a supervisor failure.
    Vanished,
    /// Same pid, same birth identity, same uid: still the very same live process,
    /// so the ambiguous read earns exactly one retry.
    SameEpochRetry,
}

/// Decide whether an ambiguous pidpath read may be retried.
///
/// Retrying is only safe while the observed process is provably the *same* one:
/// pid alone is not an identity on a system that recycles pids, so birth identity
/// and uid must both still match.  Anything else — vanished, re-used pid, new
/// owner — is reported as absent rather than retried, so a dead process can never
/// be resurrected by a second read.
#[cfg(target_os = "macos")]
fn darwin_pidpath_recheck_disposition(
    before: &ManagedProcessTopology,
    after: Option<&ManagedProcessTopology>,
) -> DarwinPidPathRecheck {
    match after {
        Some(after)
            if after.pid == before.pid
                && after.birth_identity == before.birth_identity
                && after.uid == before.uid =>
        {
            DarwinPidPathRecheck::SameEpochRetry
        }
        _ => DarwinPidPathRecheck::Vanished,
    }
}

/// One raw `proc_pidpath` read, classified.  Factored out so the D-26 recheck
/// performs the identical syscall rather than an approximation of it.
#[cfg(target_os = "macos")]
fn read_darwin_pidpath(pid_i32: i32, path: &mut [u8]) -> Result<DarwinPidPathDisposition> {
    // SAFETY: proc_pidpath is given a writable buffer with its exact capacity.
    clear_darwin_errno();
    let path_len = unsafe { proc_pidpath(pid_i32, path.as_mut_ptr().cast(), path.len() as u32) };
    let raw_os_error = darwin_errno();
    classify_darwin_pidpath_read(path_len, raw_os_error, path)
}

#[cfg(target_os = "macos")]
fn validate_darwin_full_topology_row(
    requested_pid: u32,
    actual_pid: u32,
    status: u32,
    pgid: u32,
    start_sec: u64,
    start_usec: u64,
) -> Result<()> {
    if actual_pid != requested_pid {
        bail!("PROC_PIDTBSDINFO pid mismatch: requested={requested_pid} actual={actual_pid}");
    }
    if pgid == 0 {
        bail!("PROC_PIDTBSDINFO returned zero process group");
    }
    if !(1..=5).contains(&status) {
        bail!("PROC_PIDTBSDINFO returned invalid process status {status}");
    }
    if start_sec == 0 || start_usec >= 1_000_000 {
        bail!("PROC_PIDTBSDINFO returned invalid birth time: sec={start_sec} usec={start_usec}");
    }
    Ok(())
}

/// `getsid` failure is a permitted sidless representation only for a row
/// already proven zombie. A live-row ESRCH is a disappearance race; every
/// other live failure is ambiguity.
#[cfg(target_os = "macos")]
fn classify_darwin_sid_read(
    sid: i32,
    raw_os_error: Option<i32>,
    zombie: bool,
) -> Result<Option<u32>> {
    if sid > 0 {
        if raw_os_error.unwrap_or_default() != 0 {
            bail!("getsid returned a positive SID with errno={raw_os_error:?}");
        }
        return Ok(Some(sid as u32));
    }
    if sid == 0 {
        if zombie {
            return Ok(Some(0));
        }
        bail!("getsid returned zero for live process");
    }
    if zombie {
        return Ok(Some(0));
    }
    if raw_os_error == Some(3) {
        // PROC_ALL_PIDS is an enumeration boundary, not a transaction. A
        // live row can disappear after libproc/KERN_PROC proved its identity
        // but before getsid observes it. Preserve the enumerated PID in the
        // census while omitting raced-away detail; callers that require an
        // exact row will then fail closed on listed-without-detail.
        return Ok(None);
    }
    bail!("getsid failed for live process: errno={raw_os_error:?}")
}

#[cfg(target_os = "macos")]
fn validate_darwin_kernel_process_row(
    requested_pid: i32,
    status: u8,
    actual_pid: i32,
    ppid: i32,
    pgid: i32,
    start_sec: i64,
    start_usec: i32,
) -> Result<()> {
    classify_darwin_kernel_process_kind(status)?;
    if actual_pid != requested_pid {
        bail!("KERN_PROC_PID pid mismatch: requested={requested_pid} actual={actual_pid}");
    }
    if ppid < 0 || pgid <= 0 {
        bail!("KERN_PROC_PID returned invalid parent/group: ppid={ppid} pgid={pgid}");
    }
    if start_sec <= 0 || !(0..1_000_000).contains(&start_usec) {
        bail!("KERN_PROC_PID returned invalid birth time: sec={start_sec} usec={start_usec}");
    }
    Ok(())
}

/// Decode proc_listpids' byte count without truncating a partial pid_t or
/// accepting an empty/negative whole-system census as a quiet snapshot.
#[cfg(target_os = "macos")]
fn decode_darwin_pid_list_count(
    bytes: i32,
    raw_os_error: Option<i32>,
    capacity: usize,
) -> Result<usize> {
    let width = std::mem::size_of::<i32>();
    if bytes <= 0 {
        bail!("proc_listpids returned non-positive byte count {bytes}: errno={raw_os_error:?}");
    }
    if raw_os_error.unwrap_or_default() != 0 {
        bail!("proc_listpids returned positive bytes with errno={raw_os_error:?}");
    }
    let bytes = bytes as usize;
    if bytes % width != 0 {
        bail!("proc_listpids byte count is not pid_t aligned: {bytes}");
    }
    let buffer_bytes = capacity
        .checked_mul(width)
        .context("proc_listpids buffer byte size overflow")?;
    if bytes > buffer_bytes {
        bail!(
            "proc_listpids reported bytes beyond supplied buffer: bytes={bytes} capacity={buffer_bytes}"
        );
    }
    Ok(bytes / width)
}

#[cfg(target_os = "macos")]
fn decode_darwin_group_pid_count(
    count: i32,
    raw_os_error: Option<i32>,
    capacity: usize,
) -> Result<usize> {
    if count < 0 {
        bail!("proc_listpgrppids returned negative count {count}: errno={raw_os_error:?}");
    }
    if count == 0 {
        if raw_os_error.unwrap_or_default() == 0 {
            return Ok(0);
        }
        bail!("proc_listpgrppids returned zero with errno={raw_os_error:?}");
    }
    if raw_os_error.unwrap_or_default() != 0 {
        bail!("proc_listpgrppids returned positive count with errno={raw_os_error:?}");
    }
    let count = count as usize;
    if count > capacity {
        bail!(
            "proc_listpgrppids reported count beyond supplied buffer: count={count} capacity={capacity}"
        );
    }
    Ok(count)
}

#[cfg(target_os = "macos")]
fn validate_darwin_pid_rows(label: &str, pids: &[i32]) -> Result<Vec<u32>> {
    let mut unique = BTreeSet::new();
    let mut result = Vec::with_capacity(pids.len());
    for (index, pid) in pids.iter().enumerate() {
        if *pid <= 0 {
            bail!(
                "{label} returned a non-positive declared pid {pid} at index {index}/{}",
                pids.len()
            );
        }
        let pid = u32::try_from(*pid).context("Darwin pid_t did not fit u32")?;
        if !unique.insert(pid) {
            bail!("{label} returned duplicate declared pid {pid}");
        }
        result.push(pid);
    }
    result.sort_unstable();
    Ok(result)
}

#[cfg(target_os = "macos")]
fn validate_darwin_all_pid_rows(pids: &[i32], complete: bool) -> Result<Vec<u32>> {
    // XNU's PROC_ALL_PIDS walks allproc/zombproc and includes the kernel
    // process returned by proc_getpid as PID 0. It cannot be a user provider,
    // descendant, credential, or signal target. Permit exactly that one
    // global-only kernel row; group censuses still reject every zero. A full
    // intermediate buffer may be truncated before reaching PID 0, so require
    // it only once the returned count leaves spare capacity.
    let kernel_rows = pids.iter().filter(|pid| **pid == 0).count();
    if kernel_rows > 1 || (complete && kernel_rows != 1) {
        bail!(
            "PROC_ALL_PIDS must declare exactly one kernel PID 0 in a complete census: found={kernel_rows} complete={complete}"
        );
    }
    let user_rows = pids
        .iter()
        .copied()
        .filter(|pid| *pid != 0)
        .collect::<Vec<_>>();
    validate_darwin_pid_rows("PROC_ALL_PIDS user rows", &user_rows)
}

/// Interpret only the syscall envelope around KERN_PROC_PID. Once SHORTBSDINFO
/// has exposed a kernel-visible zombie, failure to obtain a birth-bearing full
/// epoch is ambiguity and must be loud. A zero-length successful read is the
/// documented KERN_PROC_PID representation of a process that exited between
/// the SHORT and KERN snapshots, regardless of whether SHORT saw a zombie.
#[cfg(target_os = "macos")]
fn classify_darwin_zombie_kernel_read(
    has_short_zombie: bool,
    result: i32,
    raw_os_error: Option<i32>,
    size: usize,
) -> Result<DarwinKernProcDisposition> {
    if result != 0 {
        if raw_os_error == Some(3) {
            return Ok(DarwinKernProcDisposition::Missing);
        }
        bail!(
            "KERN_PROC_PID zombie proof failed{}: errno={raw_os_error:?}",
            if has_short_zombie {
                " after exact SHORTBSDINFO zombie"
            } else {
                ""
            }
        );
    }
    if raw_os_error.unwrap_or_default() != 0 {
        bail!("KERN_PROC_PID returned success with errno={raw_os_error:?}");
    }
    if size == 0 {
        return Ok(DarwinKernProcDisposition::Missing);
    }
    if size != std::mem::size_of::<KinfoProcZombie>() {
        bail!(
            "KERN_PROC_PID zombie ABI size drift{}: expected={} actual={size}",
            if has_short_zombie {
                " after exact SHORTBSDINFO zombie"
            } else {
                ""
            },
            std::mem::size_of::<KinfoProcZombie>()
        );
    }
    Ok(DarwinKernProcDisposition::Full)
}

#[cfg(target_os = "macos")]
fn validate_darwin_short_kernel_identity(
    short: Option<&DarwinShortProcessProof>,
    pid: i32,
    status: u8,
    ppid: i32,
    pgid: i32,
    uid: u32,
) -> Result<()> {
    if short.is_some_and(|short| {
        pid < 0
            || ppid < 0
            || pgid <= 0
            || short.pid != pid as u32
            || short.status != status as u32
            || short.observed_ppid != ppid as u32
            || short.pgid != pgid as u32
            || short.uid != uid
    }) {
        bail!("KERN_PROC_PID identity conflicts with exact SHORTBSDINFO zombie proof");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
extern "C" {
    fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut std::ffi::c_void, size: i32) -> i32;
    fn proc_pidinfo(
        pid: i32,
        flavor: i32,
        arg: u64,
        buffer: *mut std::ffi::c_void,
        size: i32,
    ) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
    fn proc_listpgrppids(pgrpid: i32, buffer: *mut std::ffi::c_void, size: i32) -> i32;
    fn getsid(pid: i32) -> i32;
    fn __error() -> *mut i32;
    fn sysctl(
        name: *mut i32,
        namelen: u32,
        oldp: *mut std::ffi::c_void,
        oldlenp: *mut usize,
        newp: *mut std::ffi::c_void,
        newlen: usize,
    ) -> i32;
}

#[cfg(target_os = "macos")]
fn clear_darwin_errno() {
    // SAFETY: __error returns this thread's writable errno cell on Darwin.
    unsafe { *__error() = 0 };
}

#[cfg(target_os = "macos")]
fn darwin_errno() -> Option<i32> {
    io::Error::last_os_error().raw_os_error()
}

#[cfg(target_os = "macos")]
fn inspect_darwin_full_topology(pid: u32) -> Result<DarwinFullTopologyRead> {
    let pid_i32 = checked_darwin_pid_t("topology pid", pid)?;
    let mut info = std::mem::MaybeUninit::<ProcBsdInfo>::zeroed();
    // SAFETY: libproc writes at most the exact ProcBsdInfo buffer supplied.
    clear_darwin_errno();
    let written = unsafe {
        proc_pidinfo(
            pid_i32,
            3,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<ProcBsdInfo>() as i32,
        )
    };
    // errno is thread-local but may be overwritten by the very next FFI call.
    let raw_os_error = darwin_errno();
    match classify_darwin_full_read(written, raw_os_error, std::mem::size_of::<ProcBsdInfo>())? {
        DarwinFullReadDisposition::Missing => return Ok(DarwinFullTopologyRead::Missing),
        DarwinFullReadDisposition::PermissionDenied => {
            return Ok(DarwinFullTopologyRead::PermissionDenied)
        }
        DarwinFullReadDisposition::Exact => {}
    }
    // SAFETY: exact-size successful proc_pidinfo initialized the structure.
    let info = unsafe { info.assume_init() };
    validate_darwin_full_topology_row(
        pid,
        info.pid,
        info.status,
        info.pgid,
        info.start_tvsec,
        info.start_tvusec,
    )?;
    let zombie = info.status == 5;
    // SAFETY: getsid is a read-only query for the exact pid. Darwin may reject
    // it for an unreaped zombie even though PROC_PIDTBSDINFO still supplies
    // exact pid/ppid/pgid/uid/starttime. SID=0 is an explicit zombie-only
    // marker; group membership plus an owned group-leader receipt supplies the
    // session fence during signal proof. A live row may never use the marker.
    clear_darwin_errno();
    let sid = unsafe { getsid(pid_i32) };
    // Capture errno immediately and unconditionally. A positive return with
    // non-zero errno is an invalid FFI envelope, not a success that may skip
    // validation.
    let sid_error = darwin_errno();
    let Some(sid) = classify_darwin_sid_read(sid, sid_error, zombie)? else {
        return Ok(DarwinFullTopologyRead::Missing);
    };
    let name_bytes = if info.name.first().copied().unwrap_or_default() != 0 {
        info.name.as_slice()
    } else {
        info.comm.as_slice()
    };
    let name_len = name_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name_bytes.len());
    let executable_hint = String::from_utf8_lossy(&name_bytes[..name_len]).into_owned();
    Ok(DarwinFullTopologyRead::Exact(ManagedProcessTopology {
        pid,
        observed_ppid: info.ppid,
        pgid: info.pgid,
        sid,
        uid: info.uid,
        birth_identity: format!("macos-sec-usec:{}:{}", info.start_tvsec, info.start_tvusec),
        // Darwin's proc_bsdinfo pbi_status uses SZOMB=5.
        zombie,
        executable_hint,
    }))
}

#[cfg(target_os = "macos")]
fn inspect_topology(pid: u32) -> Result<Option<ManagedProcessTopology>> {
    match inspect_darwin_full_topology(pid)? {
        DarwinFullTopologyRead::Exact(topology) => Ok(Some(topology)),
        DarwinFullTopologyRead::Missing => Ok(None),
        DarwinFullTopologyRead::PermissionDenied => {
            bail!("PROC_PIDTBSDINFO denied access to pid {pid}")
        }
    }
}

#[cfg(target_os = "macos")]
fn inspect_process(pid: u32) -> Result<Option<ManagedProcessCredential>> {
    let pid_i32 = checked_darwin_pid_t("credential pid", pid)?;
    let Some(topology) = inspect_topology(pid)? else {
        return Ok(None);
    };
    let mut path = vec![0u8; 4096];
    let path_len = match read_darwin_pidpath(pid_i32, &mut path)? {
        DarwinPidPathDisposition::Missing => return Ok(None),
        DarwinPidPathDisposition::Exact(length) => length,
        // D-26: exactly one bounded recheck, and only inside the same epoch.
        // The previous code raised an operational error here, which the wake
        // supervisor turned into a TERM — killing a primary reviewer that was
        // still working (r77/B296).  Silently treating it as "alive" would be
        // the opposite failure, so the topology decides.
        DarwinPidPathDisposition::EmptyWithoutEsrch => {
            match darwin_pidpath_recheck_disposition(&topology, inspect_topology(pid)?.as_ref()) {
                DarwinPidPathRecheck::Vanished => return Ok(None),
                DarwinPidPathRecheck::SameEpochRetry => {
                    match read_darwin_pidpath(pid_i32, &mut path)? {
                        DarwinPidPathDisposition::Missing => return Ok(None),
                        DarwinPidPathDisposition::Exact(length) => length,
                        // Still ambiguous for a process we just proved is the
                        // same live one: fail closed rather than guess.
                        DarwinPidPathDisposition::EmptyWithoutEsrch => bail!(
                            "proc_pidpath returned zero length with ENOENT twice for live pid {pid} in one epoch"
                        ),
                    }
                }
            }
        }
    };
    path.truncate(path_len);
    let executable = PathBuf::from(std::ffi::OsString::from_vec(path));
    Ok(Some(ManagedProcessCredential {
        pid: topology.pid,
        observed_ppid: topology.observed_ppid,
        pgid: topology.pgid,
        sid: topology.sid,
        uid: topology.uid,
        birth_identity: topology.birth_identity,
        executable_summary: executable_summary(&executable)?,
    }))
}

#[cfg(target_os = "macos")]
fn inspect_darwin_fallback_process(pid: u32) -> Result<Option<DarwinFallbackProcess>> {
    let pid_i32 = checked_darwin_pid_t("fallback pid", pid)?;
    let mut info = std::mem::MaybeUninit::<ProcBsdShortInfo>::zeroed();
    // SAFETY: libproc writes at most the exact short-info buffer supplied.
    clear_darwin_errno();
    let written = unsafe {
        proc_pidinfo(
            pid_i32,
            13, // PROC_PIDT_SHORTBSDINFO
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<ProcBsdShortInfo>() as i32,
        )
    };
    // Capture errno before KERN_PROC_PID or any other FFI can overwrite it.
    let short_raw_os_error = darwin_errno();
    let short_proof = match classify_darwin_short_fallback_read(
        written,
        short_raw_os_error,
        std::mem::size_of::<ProcBsdShortInfo>(),
    )? {
        DarwinFixedReadDisposition::Exact => {
            // SAFETY: exact-size successful proc_pidinfo initialized the structure.
            let info = unsafe { info.assume_init() };
            if info.pid != pid {
                bail!(
                    "PROC_PIDT_SHORTBSDINFO pid mismatch: requested={pid} actual={}",
                    info.pid
                );
            }
            if info.pgid == 0 || !(1..=5).contains(&info.status) {
                bail!(
                    "PROC_PIDT_SHORTBSDINFO returned invalid group/status: pgid={} status={}",
                    info.pgid,
                    info.status
                );
            }
            Some(DarwinShortProcessProof {
                pid: info.pid,
                observed_ppid: info.ppid,
                pgid: info.pgid,
                status: info.status,
                uid: info.uid,
            })
        }
        DarwinFixedReadDisposition::Missing => None,
    };

    // Current Darwin returns zero bytes for both PROC_PIDTBSDINFO and
    // PROC_PIDT_SHORTBSDINFO once a child is SZOMB. KERN_PROC_PID is the
    // public kernel ABI used for that exact state and retains pid/ppid/pgid,
    // uid and start time. The byte offsets below are the documented 64-bit
    // `struct kinfo_proc` layout from <sys/proc.h>/<sys/sysctl.h>; macOS Rust
    // targets are 64-bit. This fallback is zombie-only and never seeds lineage.
    let mut kinfo = std::mem::MaybeUninit::<KinfoProcZombie>::zeroed();
    let mut size = std::mem::size_of::<KinfoProcZombie>();
    let mut mib = [1i32, 14i32, 1i32, pid_i32]; // CTL_KERN/KERN_PROC/PID
                                                // SAFETY: sysctl receives the exact fixed-size public kinfo_proc buffer;
                                                // newp is null because this is a read-only query.
    clear_darwin_errno();
    let result = unsafe {
        sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            kinfo.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    let raw_os_error = darwin_errno();
    if classify_darwin_zombie_kernel_read(short_proof.is_some(), result, raw_os_error, size)?
        == DarwinKernProcDisposition::Missing
    {
        return Ok(None);
    }
    // SAFETY: exact-size successful sysctl initialized the public mirror.
    let kinfo = unsafe { kinfo.assume_init() };
    validate_darwin_kernel_process_row(
        pid_i32,
        kinfo.status,
        kinfo.pid,
        kinfo.ppid,
        kinfo.pgid,
        kinfo.start_sec,
        kinfo.start_usec,
    )?;
    validate_darwin_short_kernel_identity(
        short_proof.as_ref(),
        kinfo.pid,
        kinfo.status,
        kinfo.ppid,
        kinfo.pgid,
        kinfo.uid,
    )?;
    let birth_identity = format!("macos-sec-usec:{}:{}", kinfo.start_sec, kinfo.start_usec);
    match classify_darwin_kernel_process_kind(kinfo.status)? {
        DarwinKernelProcessKind::Zombie => {
            Ok(Some(DarwinFallbackProcess::Zombie(OpaqueZombieProof {
                pid,
                observed_ppid: kinfo.ppid as u32,
                pgid: kinfo.pgid as u32,
                uid: kinfo.uid,
                birth_identity: Some(birth_identity),
            })))
        }
        DarwinKernelProcessKind::Live => {
            // A strong live row is not disappearance. Preserve it as typed
            // topology for census/ancestry, but with no executable authority.
            // SAFETY: getsid is a read-only query for this exact strong PID.
            clear_darwin_errno();
            let sid = unsafe { getsid(pid_i32) };
            let sid_error = darwin_errno();
            let Some(sid) = classify_darwin_sid_read(sid, sid_error, false)? else {
                return Ok(None);
            };
            Ok(Some(DarwinFallbackProcess::LiveUnknown(
                ManagedProcessTopology {
                    pid,
                    observed_ppid: kinfo.ppid as u32,
                    pgid: kinfo.pgid as u32,
                    sid,
                    uid: kinfo.uid,
                    birth_identity,
                    zombie: false,
                    executable_hint: "kern-proc-live-unknown".to_string(),
                },
            )))
        }
    }
}

#[cfg(target_os = "macos")]
fn inspect_opaque_zombie(pid: u32) -> Result<Option<OpaqueZombieProof>> {
    match inspect_darwin_fallback_process(pid)? {
        Some(DarwinFallbackProcess::Zombie(zombie)) => Ok(Some(zombie)),
        Some(DarwinFallbackProcess::LiveUnknown(_)) => {
            bail!("KERN_PROC_PID proved a live process while full topology was unavailable")
        }
        None => Ok(None),
    }
}

#[cfg(target_os = "macos")]
fn record_darwin_fallback_census_row(
    pid: u32,
    fallback: Option<DarwinFallbackProcess>,
    topology: &mut BTreeMap<u32, ManagedProcessTopology>,
    opaque_zombies: &mut BTreeMap<u32, OpaqueZombieProof>,
) -> Result<()> {
    let Some(fallback) = fallback else {
        // The PID remains in ProcessTopologyCensus::listed_pids. Losing only
        // its per-process detail during the enumeration race is therefore
        // ambiguity, never evidence that a managed epoch disappeared.
        return Ok(());
    };
    match fallback {
        DarwinFallbackProcess::Zombie(zombie) => {
            if zombie.birth_identity.is_none() {
                bail!("opaque zombie pid {pid} is kernel-visible without a birth identity");
            }
            opaque_zombies.insert(pid, zombie);
        }
        DarwinFallbackProcess::LiveUnknown(row) => {
            topology.insert(pid, row);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn process_topology_census() -> Result<ProcessTopologyCensus> {
    const PROC_ALL_PIDS: u32 = 1;
    // Complete-tree custody cannot be scoped to the supervisor's current UID:
    // an unobserved descendant may cross a PGID and then exec a setuid image.
    // Enumerate the whole process table. Permission-denied or ESRCH full rows
    // are retained through the SHORT+KERN topology-only fallback below; they
    // never become executable credentials or signal authority.
    // A filled proc_listpids buffer is a truncated topology, never a valid
    // safety proof. Grow until the kernel leaves spare capacity.
    let mut capacity = 4_096usize;
    let pids = loop {
        let mut pids = vec![0i32; capacity];
        // SAFETY: libproc receives the initialized vector's writable byte range.
        let buffer_bytes = pids
            .len()
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|bytes| i32::try_from(bytes).ok())
            .context("proc_listpids buffer exceeds Darwin int")?;
        clear_darwin_errno();
        let bytes =
            unsafe { proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), buffer_bytes) };
        let raw_os_error = darwin_errno();
        let count = decode_darwin_pid_list_count(bytes, raw_os_error, pids.len())?;
        if count < pids.len() {
            pids.truncate(count);
            break validate_darwin_all_pid_rows(&pids, true)?;
        }
        validate_darwin_all_pid_rows(&pids, false)?;
        capacity = capacity
            .checked_mul(2)
            .filter(|capacity| *capacity <= 131_072)
            .context("libproc process snapshot remained truncated at safety bound")?;
    };
    let listed_pids = pids.iter().copied().collect::<BTreeSet<_>>();
    let mut topology = BTreeMap::new();
    let mut opaque_zombies = BTreeMap::new();
    for pid in pids {
        match inspect_darwin_full_topology(pid)
            .with_context(|| format!("inspect full topology for census pid {pid}"))?
        {
            DarwinFullTopologyRead::Exact(row) => {
                if let Some(sidless) = sidless_zombie_containment_proof(&row) {
                    // A full zombie can retain birth while getsid is rejected.
                    // Preserve a parallel containment-only representation;
                    // never synthesize a SID or promote it to signal authority.
                    opaque_zombies.insert(pid, sidless);
                }
                topology.insert(pid, row);
            }
            DarwinFullTopologyRead::Missing | DarwinFullTopologyRead::PermissionDenied => {
                let fallback = inspect_darwin_fallback_process(pid)
                    .with_context(|| format!("inspect fallback process for census pid {pid}"))?;
                record_darwin_fallback_census_row(
                    pid,
                    fallback,
                    &mut topology,
                    &mut opaque_zombies,
                )?;
            }
        }
    }
    Ok(ProcessTopologyCensus {
        listed_pids,
        topology,
        opaque_zombies,
    })
}

#[cfg(target_os = "macos")]
fn process_topology_snapshot() -> Result<BTreeMap<u32, ManagedProcessTopology>> {
    Ok(process_topology_census()?.topology)
}

#[cfg(not(target_os = "macos"))]
fn process_topology_census() -> Result<ProcessTopologyCensus> {
    let topology = process_topology_snapshot()?;
    let listed_pids = topology.keys().copied().collect();
    Ok(ProcessTopologyCensus {
        listed_pids,
        topology,
        opaque_zombies: BTreeMap::new(),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_topology(_pid: u32) -> Result<Option<ManagedProcessTopology>> {
    bail!("managed process topology unsupported on this platform")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_process(_pid: u32) -> Result<Option<ManagedProcessCredential>> {
    bail!("managed process credentials unsupported on this platform")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_topology_snapshot() -> Result<BTreeMap<u32, ManagedProcessTopology>> {
    bail!("managed process ancestry unsupported on this platform")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_opaque_zombie(_pid: u32) -> Result<Option<OpaqueZombieProof>> {
    bail!("managed zombie inspection unsupported on this platform")
}

fn lineage_pids(
    provider_pid: u32,
    isolated_sid: Option<u32>,
    owned: &BTreeMap<u32, ManagedProcessCredential>,
    snapshot: &BTreeMap<u32, ManagedProcessTopology>,
) -> Result<BTreeSet<u32>> {
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for topology in snapshot.values() {
        children
            .entry(topology.observed_ppid)
            .or_default()
            .push(topology.pid);
    }
    let mut lineage = BTreeSet::new();
    let mut frontier = std::collections::VecDeque::new();
    for (pid, receipt) in owned {
        if snapshot
            .get(pid)
            .is_some_and(|topology| owned_lineage_root_matches(receipt, topology, isolated_sid))
        {
            lineage.insert(*pid);
            frontier.push_back(*pid);
        }
    }
    while let Some(parent) = frontier.pop_front() {
        let Some(descendants) = children.get(&parent) else {
            continue;
        };
        for pid in descendants {
            if lineage.insert(*pid) {
                frontier.push_back(*pid);
            }
        }
    }
    if let Some(sid) = isolated_sid {
        // A numeric SID is reusable after the original session disappears.
        // It may widen discovery only while a currently observed owned epoch
        // still anchors that session. Receipt-root BFS above remains valid
        // without this anchor because it starts from an exact current PID,
        // UID and birth identity.
        let mut current_session_anchor = owned.iter().any(|(pid, receipt)| {
            receipt.sid == sid
                && snapshot.get(pid).is_some_and(|topology| {
                    topology.sid == sid
                        && receipt_immutable_matches_topology(receipt, topology)
                        && (!is_zombie_provider_epoch(receipt) || topology.zombie)
                })
        });
        if !current_session_anchor {
            for (pid, receipt) in owned.iter().filter(|(_, receipt)| receipt.sid == sid) {
                let topology_is_missing_or_sidless_zombie = snapshot
                    .get(pid)
                    .map_or(true, |topology| topology.zombie && topology.sid == 0);
                if !topology_is_missing_or_sidless_zombie {
                    continue;
                }
                // Darwin may hide an unreaped zombie from full BSD info or
                // report SID=0. Only KERN_PROC_PID's exact birth-bearing
                // opaque proof may keep that already-owned epoch as an SID
                // anchor. A structural SHORTBSDINFO proof never matches.
                if inspect_opaque_zombie(*pid)?
                    .as_ref()
                    .is_some_and(|zombie| opaque_zombie_matches_receipt(zombie, receipt))
                {
                    current_session_anchor = true;
                    break;
                }
            }
        }
        if current_session_anchor {
            lineage.extend(
                snapshot
                    .values()
                    .filter(|topology| topology.sid == sid)
                    .map(|topology| topology.pid),
            );
        }
    }
    lineage.remove(&provider_pid);
    Ok(lineage)
}

fn baseline_identity_matches(
    baseline: &ManagedProcessTopology,
    current: &ManagedProcessTopology,
) -> bool {
    baseline.pid == current.pid && baseline.birth_identity == current.birth_identity
}

/// Kernel epoch continuity is only PID plus the non-reusable birth identity.
/// UID/PGID/SID are signal-authority fields: a live process can change them
/// without becoming a different epoch, so drift must preserve custody while
/// revoking use of the previously captured credential.
fn receipt_epoch_matches_topology(
    receipt: &ManagedProcessCredential,
    topology: &ManagedProcessTopology,
) -> bool {
    receipt.pid == topology.pid && receipt.birth_identity == topology.birth_identity
}

fn receipt_epoch_matches_opaque_zombie(
    receipt: &ManagedProcessCredential,
    zombie: &OpaqueZombieProof,
) -> bool {
    zombie.pid == receipt.pid
        && zombie
            .birth_identity
            .as_ref()
            .is_some_and(|birth| birth == &receipt.birth_identity)
}

/// A zombie has no executable image to signal. Exact current group binding
/// plus PID/UID/birth continuity may therefore carry an already-owned zombie
/// across a PGID/SID escape as a no-signal obligation. This predicate must not
/// authorize a live process or an isolated-session lineage anchor.
fn owned_zombie_epoch_matches_receipt(
    receipt: &ManagedProcessCredential,
    zombie: &OpaqueZombieProof,
) -> bool {
    receipt.uid == zombie.uid && receipt_epoch_matches_opaque_zombie(receipt, zombie)
}

/// This stricter predicate is a signal/lineage credential component, not an
/// epoch-continuity test. In particular UID drift must not retire custody.
fn receipt_immutable_matches_topology(
    receipt: &ManagedProcessCredential,
    topology: &ManagedProcessTopology,
) -> bool {
    receipt.pid == topology.pid
        && receipt.uid == topology.uid
        && receipt.birth_identity == topology.birth_identity
}

fn owned_lineage_root_matches(
    receipt: &ManagedProcessCredential,
    topology: &ManagedProcessTopology,
    _isolated_sid: Option<u32>,
) -> bool {
    // An already-proven child remains the same owned process across
    // exec/setpgid/setsid/reparent. Only kernel-backed immutable identity may
    // seed another BFS; a reused PID with a different birth can never expand
    // lineage.
    receipt_immutable_matches_topology(receipt, topology)
        && (!is_zombie_provider_epoch(receipt) || topology.zombie)
}

fn credential_matches_topology(
    credential: &ManagedProcessCredential,
    topology: &ManagedProcessTopology,
) -> bool {
    credential.pid == topology.pid
        && credential.observed_ppid == topology.observed_ppid
        && credential.pgid == topology.pgid
        && credential.sid == topology.sid
        && credential.uid == topology.uid
        && credential.birth_identity == topology.birth_identity
}

fn credential_identity_matches(
    expected: &ManagedProcessCredential,
    current: &ManagedProcessCredential,
) -> bool {
    expected.pid == current.pid
        && expected.pgid == current.pgid
        && expected.sid == current.sid
        && expected.uid == current.uid
        && expected.birth_identity == current.birth_identity
        && expected.executable_summary == current.executable_summary
}

/// ACCEPT binds the complete OFFER credential, including the executable.
/// Cleanup has a narrower need: after rejecting a same-PID exec race, it may
/// replace only the executable part of the receipt so the already-owned
/// direct child can still be terminated.  Every kernel-backed epoch and
/// containment field must remain identical; this must never authorize ACCEPT.
fn launch_guard_cleanup_refresh_matches(
    offered: &ManagedProcessCredential,
    fresh: &ManagedProcessCredential,
) -> bool {
    offered.pid == fresh.pid
        && offered.observed_ppid == fresh.observed_ppid
        && offered.pgid == fresh.pgid
        && offered.sid == fresh.sid
        && offered.uid == fresh.uid
        && offered.birth_identity == fresh.birth_identity
}

fn credential_identity_matches_topology(
    credential: &ManagedProcessCredential,
    topology: &ManagedProcessTopology,
) -> bool {
    credential.pid == topology.pid
        && credential.pgid == topology.pgid
        && credential.sid == topology.sid
        && credential.uid == topology.uid
        && credential.birth_identity == topology.birth_identity
}

fn candidate_topologies(
    provider_pid: u32,
    baseline: &BTreeMap<u32, ManagedProcessTopology>,
    owned: &BTreeMap<u32, ManagedProcessCredential>,
    snapshot: &BTreeMap<u32, ManagedProcessTopology>,
    lineage: &BTreeSet<u32>,
) -> Vec<ManagedProcessTopology> {
    snapshot
        .values()
        .filter(|topology| topology.pid != provider_pid)
        .filter(|topology| !owned.contains_key(&topology.pid))
        .filter(|topology| {
            !baseline
                .get(&topology.pid)
                .is_some_and(|old| baseline_identity_matches(old, topology))
        })
        .filter(|topology| lineage.contains(&topology.pid))
        .cloned()
        .collect()
}

fn candidate_promotion_is_stable(
    baseline: &BTreeMap<u32, ManagedProcessTopology>,
    before: &ManagedProcessTopology,
    credential: &ManagedProcessCredential,
    after_snapshot: &BTreeMap<u32, ManagedProcessTopology>,
    after_lineage: &BTreeSet<u32>,
) -> bool {
    let Some(after) = after_snapshot.get(&before.pid) else {
        return false;
    };
    before == after
        && credential_matches_topology(credential, after)
        && !baseline
            .get(&after.pid)
            .is_some_and(|old| baseline_identity_matches(old, after))
        && after_lineage.contains(&after.pid)
}

fn owned_helper_refresh_is_stable(
    expected: &ManagedProcessCredential,
    before: &ManagedProcessTopology,
    credential: &ManagedProcessCredential,
    after_snapshot: &BTreeMap<u32, ManagedProcessTopology>,
    after_lineage: &BTreeSet<u32>,
    _isolated_sid: Option<u32>,
) -> bool {
    let Some(after) = after_snapshot.get(&before.pid) else {
        return false;
    };
    before == after
        && receipt_immutable_matches_topology(expected, before)
        && expected.pid == credential.pid
        && expected.uid == credential.uid
        && expected.birth_identity == credential.birth_identity
        && credential_matches_topology(credential, after)
        && after_lineage.contains(&after.pid)
}

#[derive(Debug, Default, Clone, Copy)]
struct ProcessObserverMetrics {
    topology_snapshots: usize,
    full_credential_inspections: usize,
}

/// Per-supervisor observer.  Metrics and fault injection are local, so tests
/// do not race through process-global counters when the workspace runs in
/// parallel.
#[derive(Default)]
struct ProcessObserver {
    metrics: ProcessObserverMetrics,
    diagnostics: ManagedObservationDiagnostics,
    #[cfg(test)]
    fault_notify: Option<std::sync::mpsc::SyncSender<&'static str>>,
    #[cfg(test)]
    fail_topology_at: Option<usize>,
    #[cfg(test)]
    fail_full_at: Option<usize>,
    #[cfg(test)]
    scheduled_observation_notify:
        Option<std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>>,
    #[cfg(test)]
    owned_promotion_notify:
        Option<std::sync::Arc<(std::sync::Mutex<BTreeSet<u32>>, std::sync::Condvar)>>,
    #[cfg(test)]
    force_opaque_pid: Option<std::sync::Arc<std::sync::atomic::AtomicU32>>,
}

impl ProcessObserver {
    fn record_observation_failure(&mut self, stage: &str, error: &anyhow::Error) {
        self.diagnostics.failures = self.diagnostics.failures.saturating_add(1);
        let message: String = crate::redact::redact_full(&format!("{stage}: {error:#}"))
            .chars().take(512).collect();
        if self.diagnostics.samples.len() < 32 && !self.diagnostics.samples.contains(&message) {
            eprintln!("orch: process observation unavailable; no stop or signal authority: {message}");
            self.diagnostics.samples.push(message);
        }
    }

    fn topology_census(&mut self) -> Result<ProcessTopologyCensus> {
        self.metrics.topology_snapshots += 1;
        #[cfg(test)]
        if self.fail_topology_at == Some(self.metrics.topology_snapshots) {
            if let Some(notify) = &self.fault_notify { let _ = notify.try_send("census"); }
            bail!("injected process-topology snapshot failure");
        }
        #[allow(unused_mut)] // mutated only by deterministic test injection
        let mut census = process_topology_census()?;
        #[cfg(test)]
        if let Some(pid) = self
            .force_opaque_pid
            .as_ref()
            .map(|pid| pid.load(std::sync::atomic::Ordering::SeqCst))
            .filter(|pid| *pid != 0)
        {
            census.topology.remove(&pid);
            census.opaque_zombies.remove(&pid);
            if let Some(zombie) = inspect_opaque_zombie(pid)? {
                if zombie.birth_identity.is_none() {
                    bail!("forced opaque zombie pid {pid} lacks a birth identity");
                }
                census.opaque_zombies.insert(pid, zombie);
            }
        }
        Ok(census)
    }

    fn notify_scheduled_observation_complete(&self) {
        #[cfg(test)]
        if let Some(notify) = &self.scheduled_observation_notify {
            let (count, condition) = &**notify;
            let mut count = count
                .lock()
                .expect("scheduled observation notification mutex poisoned");
            *count += 1;
            condition.notify_all();
        }
    }

    fn notify_owned_promotion(&self, owned_helpers: &BTreeSet<u32>) {
        #[cfg(test)]
        if !owned_helpers.is_empty() {
            if let Some(notify) = &self.owned_promotion_notify {
                let (pids, condition) = &**notify;
                let mut pids = pids
                    .lock()
                    .expect("owned-promotion notification mutex poisoned");
                pids.extend(owned_helpers);
                condition.notify_all();
            }
        }
        #[cfg(not(test))]
        let _ = owned_helpers;
    }

    fn topology_snapshot(&mut self) -> Result<BTreeMap<u32, ManagedProcessTopology>> {
        Ok(self.topology_census()?.topology)
    }

    fn stable_credential(&mut self, pid: u32) -> Result<Option<ManagedProcessCredential>> {
        self.metrics.full_credential_inspections += 1;
        #[cfg(test)]
        if self.fail_full_at == Some(self.metrics.full_credential_inspections) {
            if let Some(notify) = &self.fault_notify { let _ = notify.try_send("credential"); }
            bail!("injected full-credential inspection failure");
        }
        inspect_stable_process(pid)
    }
}

/// Discover candidates from one light snapshot, fully inspect only those
/// candidates, then bind every promotion to a second light snapshot.  The A /
/// full / B sandwich rejects PID reuse and ancestry changes without turning a
/// full-system executable digest into a 20ms polling cost.
fn observe_owned_helpers(
    observer: &mut ProcessObserver,
    provider_pid: u32,
    isolated_sid: Option<u32>,
    baseline: &BTreeMap<u32, ManagedProcessTopology>,
    owned: &mut BTreeMap<u32, ManagedProcessCredential>,
    zombie_containment: &mut BTreeMap<u32, ZombieContainmentReceipt>,
) -> Result<BTreeMap<u32, ManagedProcessTopology>> {
    let before_census = observer.topology_census()?;
    let before_epochs = normalized_zombie_epochs(&before_census)?;
    let before = &before_census.topology;
    let before_lineage = lineage_pids(provider_pid, isolated_sid, owned, before)?;
    let candidates = candidate_topologies(provider_pid, baseline, owned, before, &before_lineage);
    let refresh_candidates = owned
        .iter()
        .filter(|(pid, _)| **pid != provider_pid)
        .filter_map(|(pid, expected)| {
            let topology = before.get(pid)?;
            (before_lineage.contains(pid)
                && owned_lineage_root_matches(expected, topology, isolated_sid))
            .then(|| (expected.clone(), topology.clone()))
        })
        .collect::<Vec<_>>();

    // An unowned zombie cannot yield an executable credential. Capture only
    // a non-signal containment receipt while the provider's current full
    // credential still anchors the freshly isolated SID. The receipt is used
    // solely to wait for provider reap; it never enters `owned`.
    let has_zombie_candidate = isolated_sid.is_some()
        && before_epochs.values().any(|epoch| {
            epoch.pid != provider_pid
                && epoch.pgid != provider_pid
                && epoch.observed_ppid == provider_pid
                && !owned.contains_key(&epoch.pid)
                && baseline.get(&epoch.pid).is_none_or(|old| {
                    old.pid != epoch.pid
                        || old.uid != epoch.uid
                        || old.birth_identity != epoch.birth_identity
                })
        });
    let fresh_provider = if has_zombie_candidate {
        observer.stable_credential(provider_pid)?
    } else {
        None
    };

    let mut inspected = BTreeMap::new();
    for pid in candidates
        .iter()
        .map(|candidate| candidate.pid)
        .chain(refresh_candidates.iter().map(|(_, topology)| topology.pid))
        .collect::<BTreeSet<_>>()
    {
        if let Some(credential) = observer.stable_credential(pid)? {
            inspected.insert(pid, credential);
        }
    }
    let after_census = observer.topology_census()?;
    let after_epochs = normalized_zombie_epochs(&after_census)?;
    let after = &after_census.topology;
    let after_lineage = lineage_pids(provider_pid, isolated_sid, owned, after)?;
    for candidate in candidates {
        let Some(current) = after.get(&candidate.pid) else {
            continue;
        };
        let Some(credential) = inspected.remove(&candidate.pid) else {
            continue;
        };
        if !candidate_promotion_is_stable(baseline, &candidate, &credential, after, &after_lineage)
        {
            continue;
        }
        owned.insert(current.pid, credential);
    }
    for (expected, before_topology) in refresh_candidates {
        let Some(credential) = inspected.remove(&before_topology.pid) else {
            continue;
        };
        if owned_helper_refresh_is_stable(
            &expected,
            &before_topology,
            &credential,
            after,
            &after_lineage,
            isolated_sid,
        ) {
            owned.insert(credential.pid, credential);
        }
    }

    if let (Some(sid), Some(expected_provider), Some(fresh_provider)) = (
        isolated_sid,
        owned.get(&provider_pid),
        fresh_provider.as_ref(),
    ) {
        let provider_anchor_is_current = *fresh_provider == *expected_provider
            && before.get(&provider_pid).is_some_and(|topology| {
                topology.sid == sid && credential_matches_topology(fresh_provider, topology)
            })
            && after.get(&provider_pid).is_some_and(|topology| {
                topology.sid == sid && credential_matches_topology(fresh_provider, topology)
            });
        if provider_anchor_is_current {
            for (pid, first) in &before_epochs {
                let Some(second) = after_epochs.get(pid) else {
                    continue;
                };
                let Some(receipt) = stable_containment_creation(
                    first,
                    second,
                    provider_pid,
                    sid,
                    expected_provider.uid,
                ) else {
                    continue;
                };
                if !owned.contains_key(pid)
                    && !zombie_containment.contains_key(pid)
                    && baseline.get(pid).is_none_or(|old| {
                        old.pid != receipt.pid
                            || old.uid != receipt.uid
                            || old.birth_identity != receipt.birth_identity
                    })
                {
                    zombie_containment.insert(*pid, receipt);
                }
            }
        }
    }

    // A captured receipt is a durable non-signal discharge obligation. Do
    // not silently drop it on reparent, same-birth structural change, or a
    // declared row whose detail raced unavailable. Only stable exact absence
    // or a stable different birth epoch retires the old custody record.
    let mut retired_containment = Vec::new();
    for (pid, receipt) in zombie_containment.iter() {
        if custody_epoch_retired_by_stable_pair(receipt, &before_census, &after_census)? {
            retired_containment.push(*pid);
        }
    }
    for pid in retired_containment {
        zombie_containment.remove(&pid);
    }
    // `owned` is both a signal credential cache and the durable custody set.
    // UID/PGID/SID drift invalidates the former but not the latter. Retire the
    // old epoch only after stable A/B absence or one stable replacement birth;
    // a PID declared without detail is explicitly not absence.
    let mut retired = Vec::new();
    for (pid, receipt) in owned.iter().filter(|(pid, _)| **pid != provider_pid) {
        if owned_epoch_retired_by_stable_pair(receipt, &before_census, &after_census)? {
            retired.push(*pid);
        }
    }
    for pid in retired {
        owned.remove(&pid);
    }
    Ok(after_census.topology)
}

#[doc(hidden)]
pub const PROVIDER_IDENTITY_STABLE_MS: u64 = 50;

#[doc(hidden)]
pub const PROVIDER_IDENTITY_SETTLE_WINDOW_MS: u64 = 500;

fn capture_provider_credential(
    observer: &mut ProcessObserver,
    provider_pid: u32,
) -> Result<ManagedProcessCredential> {
    capture_provider_credential_in_window(observer, provider_pid, None)
}

// macOS /bin/sh can dispatch through the administrator-selected shell. Do not
// publish that measured intermediate image as an OFFER. This is not an
// equivalence between images: every later comparison stays byte-for-byte exact.
#[cfg(target_os = "macos")]
fn initial_shell_dispatch_image() -> Result<Option<String>> {
    let selected = match fs::canonicalize("/var/select/sh") {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read system shell selector"),
    };
    let dispatch = fs::canonicalize("/bin/sh")?;
    if selected == dispatch { return Ok(None); }
    Ok(Some(executable_summary(&dispatch)?))
}
#[cfg(not(target_os = "macos"))]
fn initial_shell_dispatch_image() -> Result<Option<String>> { Ok(None) }

fn capture_initial_provider_credential(
    observer: &mut ProcessObserver,
    provider_pid: u32,
) -> Result<ManagedProcessCredential> {
    let dispatch = initial_shell_dispatch_image()?;
    capture_provider_credential_in_window(observer, provider_pid, dispatch.as_deref())
}

// Revalidate a previously settled OFFER with a fresh A/full/B observation.
// The caller still compares every field to the OFFER. Repeating the startup
// wait here would consume short-lived providers without strengthening equality.
fn sample_provider_credential(
    observer: &mut ProcessObserver,
    provider_pid: u32,
) -> Result<ManagedProcessCredential> {
    let before = observer.topology_snapshot()?;
    let first = before
        .get(&provider_pid)
        .cloned()
        .context("provider missing from topology A")?;
    let credential = observer
        .stable_credential(provider_pid)?
        .context("provider full credential did not stabilize")?;
    let after = observer.topology_snapshot()?;
    let second = after
        .get(&provider_pid)
        .context("provider missing from topology B")?;
    if first != *second
        || !credential_matches_topology(&credential, second)
        || credential.pid != provider_pid
        || credential.pgid != provider_pid
    {
        bail!("provider A/full/B identity or PID=PGID proof failed");
    }
    Ok(credential)
}

fn capture_provider_credential_in_window(
    observer: &mut ProcessObserver,
    provider_pid: u32,
    excluded_dispatch_image: Option<&str>,
) -> Result<ManagedProcessCredential> {
    // Script-backed test adapters and portable launch shims may perform one
    // same-PID exec immediately after Command::spawn returns. Never relax the
    // immutable A/full/B proof; retry the entire proof for a short bounded
    // settling window instead.
    let deadline = Instant::now() + Duration::from_millis(PROVIDER_IDENTITY_SETTLE_WINDOW_MS);
    let required_stability = Duration::from_millis(PROVIDER_IDENTITY_STABLE_MS);
    let mut stable: Option<(ManagedProcessCredential, Instant)> = None;
    loop {
        let proof = sample_provider_credential(observer, provider_pid);
        let error = match proof {
            Ok(credential) if excluded_dispatch_image == Some(credential.executable_summary.as_str()) => {
                stable = None;
                Some(anyhow::anyhow!("provider is still executing the system shell dispatch image"))
            }
            Ok(credential) => {
                if let Some((previous, since)) = stable.as_ref() {
                    if *previous == credential
                        && since.elapsed() >= required_stability
                        && Instant::now() <= deadline
                    {
                        return Ok(credential);
                    }
                }
                if !stable
                    .as_ref()
                    .is_some_and(|(previous, _)| *previous == credential)
                {
                    stable = Some((credential, Instant::now()));
                }
                None
            }
            Err(error) => {
                stable = None;
                Some(error)
            }
        };
        if Instant::now() >= deadline {
            if let Some(error) = error {
                bail!(
                    "provider identity did not stabilize within {}ms: {error:#}",
                    PROVIDER_IDENTITY_SETTLE_WINDOW_MS
                );
            }
            bail!(
                "provider identity was not unchanged for {}ms within the {}ms settle window",
                PROVIDER_IDENTITY_STABLE_MS,
                PROVIDER_IDENTITY_SETTLE_WINDOW_MS
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn configure_isolated_session(command: &mut Command) {
    // SAFETY: setsid(2) is async-signal-safe and the closure performs no
    // allocation or locking between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

fn process_group_alive_exact(pgid: u32) -> Result<bool> {
    if pgid == 0 {
        bail!("process group id must be non-zero");
    }
    let pgid = i32::try_from(pgid).context("process group id exceeds pid_t")?;
    // SAFETY: negative pid selects exactly one process group; signal 0 is a
    // read-only existence/permission probe.
    if unsafe { libc_kill(-pgid, 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(3) => Ok(false), // ESRCH: the only proof that the group is gone.
        Some(1) => Ok(true),  // EPERM: it exists (macOS returns this for zombies).
        _ => Err(error).context("query exact process group with kill(2) failed"),
    }
}

#[cfg(target_os = "macos")]
fn exact_group_member_pids(pgid: u32) -> Result<Vec<u32>> {
    let pgid = checked_darwin_pid_t("process group id", pgid)?;
    let mut capacity = 32usize;
    loop {
        let mut pids = vec![0i32; capacity];
        let buffer_bytes = pids
            .len()
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|bytes| i32::try_from(bytes).ok())
            .context("proc_listpgrppids buffer exceeds Darwin int")?;
        // SAFETY: libproc receives the initialized vector's writable byte range.
        // libproc maps __proc_info(-1) to zero, so errno must be cleared and
        // captured around this exact call to distinguish empty from error.
        clear_darwin_errno();
        let count = unsafe { proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), buffer_bytes) };
        let raw_os_error = darwin_errno();
        // Unlike proc_listpids, the proc_listpgrppids wrapper already divides
        // the kernel byte count by sizeof(pid_t) and returns a PID count.
        let count = decode_darwin_group_pid_count(count, raw_os_error, pids.len())?;
        if count < pids.len() {
            pids.truncate(count);
            return validate_darwin_pid_rows("proc_listpgrppids", &pids);
        }
        validate_darwin_pid_rows("proc_listpgrppids full buffer", &pids)?;
        capacity = capacity
            .checked_mul(2)
            .filter(|capacity| *capacity <= 16_384)
            .context("exact process-group census remained truncated at safety bound")?;
    }
}

#[cfg(not(target_os = "macos"))]
fn exact_group_member_pids(pgid: u32) -> Result<Vec<u32>> {
    let mut result = process_topology_snapshot()?
        .values()
        .filter(|topology| topology.pgid == pgid)
        .map(|topology| topology.pid)
        .collect::<Vec<_>>();
    result.sort_unstable();
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupCredentialProof {
    AlreadyGone,
    OwnedLive,
    /// Every exact member is an immutable-matching owned zombie (or an
    /// explicitly proven Darwin SZOMB under an owned isolated leader). There
    /// is no executable image left to signal; the provider must be terminated
    /// and reaped before this helper group can disappear.
    OwnedZombieOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExactGroupMemberObservation {
    Live(ManagedProcessTopology),
    /// Canonical no-signal zombie epoch. Full and opaque Darwin rows collapse
    /// to this representation; SID is intentionally omitted because opaque
    /// proof cannot recover it and a zombie never receives signal authority.
    Zombie(OpaqueZombieProof),
}

fn exact_group_member_observation(
    census: &ProcessTopologyCensus,
    listed_pid: u32,
    target_pgid: u32,
) -> Result<ExactGroupMemberObservation> {
    if let Some(zombie) = normalized_zombie_epoch(census, listed_pid)? {
        if zombie.pid != listed_pid || zombie.pgid != target_pgid {
            bail!(
                "exact-list pid {listed_pid} resolved to pid/pgid {}/{} instead of target {target_pgid}",
                zombie.pid,
                zombie.pgid
            );
        }
        return Ok(ExactGroupMemberObservation::Zombie(OpaqueZombieProof {
            pid: zombie.pid,
            observed_ppid: zombie.observed_ppid,
            pgid: zombie.pgid,
            uid: zombie.uid,
            birth_identity: Some(zombie.birth_identity),
        }));
    }
    if let Some(topology) = census.topology.get(&listed_pid) {
        if topology.zombie {
            bail!("exact-list zombie pid {listed_pid} did not normalize to an epoch");
        }
        if topology.pid != listed_pid || topology.pgid != target_pgid {
            bail!(
                "exact-list pid {listed_pid} resolved to pid/pgid {}/{} instead of target {target_pgid}",
                topology.pid,
                topology.pgid
            );
        }
        return Ok(ExactGroupMemberObservation::Live(topology.clone()));
    }
    bail!("exact-list pid {listed_pid} has neither full topology nor birth-bearing opaque detail")
}

trait ManagedGroupControl {
    fn prove(
        &mut self,
        pgid: u32,
        owned: &BTreeMap<u32, ManagedProcessCredential>,
    ) -> std::result::Result<GroupCredentialProof, Vec<String>>;
    fn send(&mut self, pgid: u32, signal: super::process::TerminationSignal) -> Result<()>;
    fn wait_until_gone(&mut self, pgid: u32, grace: Duration) -> Result<bool>;
    /// Reap a direct group leader only after a fresh exact proof established
    /// that every remaining member is a zombie. The default covers helper
    /// groups, which have no Child handle in this control.
    fn reap_direct_if_zombie_only(&mut self, _pgid: u32) -> Result<bool> {
        Ok(false)
    }
}

struct SystemGroupControl<'a> {
    observer: &'a mut ProcessObserver,
    direct_child: Option<(&'a mut Child, &'a mut Option<ExitStatus>)>,
}

impl ManagedGroupControl for SystemGroupControl<'_> {
    fn prove(
        &mut self,
        pgid: u32,
        owned: &BTreeMap<u32, ManagedProcessCredential>,
    ) -> std::result::Result<GroupCredentialProof, Vec<String>> {
        revalidate_group_credentials(self.observer, pgid, owned)
    }

    fn send(&mut self, pgid: u32, signal: super::process::TerminationSignal) -> Result<()> {
        let (name, number) = match signal {
            super::process::TerminationSignal::Term => ("TERM", 15),
            super::process::TerminationSignal::Kill => ("KILL", 9),
        };
        let pid = i32::try_from(pgid).context("process group id exceeds pid_t")?;
        // SAFETY: negative pid targets the exact previously revalidated group.
        if unsafe { libc_kill(-pid, number) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("send {name} to exact pgid {pgid} failed"));
        }
        Ok(())
    }

    fn wait_until_gone(&mut self, pgid: u32, grace: Duration) -> Result<bool> {
        let deadline = Instant::now() + grace;
        loop {
            if !process_group_alive_exact(pgid)? {
                if let Some((child, status)) = self.direct_child.as_mut() {
                    if status.is_none() {
                        **status = child
                            .try_wait()
                            .context("reap direct provider after exact group exit")?;
                    }
                }
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10).min(grace));
        }
    }

    fn reap_direct_if_zombie_only(&mut self, pgid: u32) -> Result<bool> {
        let Some((child, status)) = self.direct_child.as_mut() else {
            return Ok(false);
        };
        if child.id() != pgid {
            bail!("direct Child identity does not match the exact zombie-only group");
        }
        if status.is_none() {
            let exited = child
                .try_wait()
                .context("reap direct leader after exact zombie-only proof")?
                .context("exact zombie-only group still had a live direct leader")?;
            **status = Some(exited);
        }
        Ok(!process_group_alive_exact(pgid)?)
    }
}

#[derive(Debug)]
struct RevalidatedGroupTermination {
    signals: Vec<super::process::TerminationSignal>,
    /// Physical KILL deliveries may repeat inside one bounded convergence
    /// window. `signals` remains the semantic TERM/KILL sequence exposed to
    /// callers, while this counter preserves the retry evidence for tests and
    /// diagnostics.
    kill_attempts: usize,
    terminated: bool,
    zombie_awaiting_provider_reap: bool,
    convergence_budget_exhausted: bool,
    errors: Vec<String>,
}

fn settle_owned_zombie_only(
    control: &mut impl ManagedGroupControl,
    pgid: u32,
    outcome: &mut RevalidatedGroupTermination,
) {
    match control.reap_direct_if_zombie_only(pgid) {
        Ok(true) => outcome.terminated = true,
        Ok(false) => outcome.zombie_awaiting_provider_reap = true,
        Err(error) => outcome.errors.push(format!(
            "pgid {pgid} direct zombie-only reap failed: {error:#}"
        )),
    }
}

/// TERM and KILL are two independent authorities. Numeric PGIDs are reusable,
/// so a proof captured before TERM cannot authorize KILL after the grace
/// window. The second signal is sent only after a fresh exact census and
/// A/full/B credential proof over every signalable member.
fn terminate_revalidated_group(
    control: &mut impl ManagedGroupControl,
    pgid: u32,
    owned: &BTreeMap<u32, ManagedProcessCredential>,
    grace: Duration,
) -> RevalidatedGroupTermination {
    terminate_revalidated_group_until(control, pgid, owned, grace, None)
}

fn termination_signal_budget_exhausted(
    outcome: &mut RevalidatedGroupTermination,
    pgid: u32,
    signal: &str,
    signal_deadline: Option<Instant>,
) -> bool {
    if !signal_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return false;
    }
    outcome.convergence_budget_exhausted = true;
    outcome.errors.push(format!(
        "pgid {pgid} residual convergence budget exhausted; {signal} refused"
    ));
    true
}

fn grace_before_deadline(grace: Duration, signal_deadline: Option<Instant>) -> Duration {
    signal_deadline.map_or(grace, |deadline| {
        grace.min(deadline.saturating_duration_since(Instant::now()))
    })
}

fn terminate_revalidated_group_until(
    control: &mut impl ManagedGroupControl,
    pgid: u32,
    owned: &BTreeMap<u32, ManagedProcessCredential>,
    grace: Duration,
    signal_deadline: Option<Instant>,
) -> RevalidatedGroupTermination {
    let mut outcome = RevalidatedGroupTermination {
        signals: Vec::new(),
        kill_attempts: 0,
        terminated: false,
        zombie_awaiting_provider_reap: false,
        convergence_budget_exhausted: false,
        errors: Vec::new(),
    };
    match control.prove(pgid, owned) {
        Ok(GroupCredentialProof::AlreadyGone) => {
            outcome.terminated = true;
            return outcome;
        }
        Ok(GroupCredentialProof::OwnedLive) => {}
        Ok(GroupCredentialProof::OwnedZombieOnly) => {
            settle_owned_zombie_only(control, pgid, &mut outcome);
            return outcome;
        }
        Err(errors) => {
            outcome.errors.extend(errors);
            return outcome;
        }
    }
    if termination_signal_budget_exhausted(&mut outcome, pgid, "TERM", signal_deadline) {
        return outcome;
    }
    match control.send(pgid, super::process::TerminationSignal::Term) {
        Ok(()) => outcome.signals.push(super::process::TerminationSignal::Term),
        Err(signal_error) => {
            match control.wait_until_gone(pgid, Duration::ZERO) {
                Ok(true) => outcome.terminated = true,
                Ok(false) | Err(_) => outcome.errors.push(format!(
                    "pgid {pgid} TERM failed before convergence: {signal_error:#}"
                )),
            }
            return outcome;
        }
    }
    match control.wait_until_gone(pgid, grace_before_deadline(grace, signal_deadline)) {
        Ok(true) => {
            outcome.terminated = true;
            return outcome;
        }
        Ok(false) => {}
        Err(wait_error) => {
            outcome.errors.push(format!(
                "pgid {pgid} liveness failed after TERM; KILL refused: {wait_error:#}"
            ));
            return outcome;
        }
    }
    match control.prove(pgid, owned) {
        Ok(GroupCredentialProof::AlreadyGone) => {
            outcome.terminated = true;
            return outcome;
        }
        Ok(GroupCredentialProof::OwnedLive) => {}
        Ok(GroupCredentialProof::OwnedZombieOnly) => {
            settle_owned_zombie_only(control, pgid, &mut outcome);
            return outcome;
        }
        Err(errors) => {
            outcome.errors.extend(errors);
            outcome.errors.push(format!(
                "pgid {pgid} identity changed or became ambiguous after TERM; KILL refused"
            ));
            return outcome;
        }
    }
    if termination_signal_budget_exhausted(&mut outcome, pgid, "KILL", signal_deadline) {
        return outcome;
    }
    outcome.kill_attempts += 1;
    match control.send(pgid, super::process::TerminationSignal::Kill) {
        Ok(()) => outcome.signals.push(super::process::TerminationSignal::Kill),
        Err(signal_error) => {
            match control.wait_until_gone(pgid, Duration::ZERO) {
                Ok(true) => outcome.terminated = true,
                Ok(false) | Err(_) => outcome.errors.push(format!(
                    "pgid {pgid} KILL failed before convergence: {signal_error:#}"
                )),
            }
            return outcome;
        }
    }
    // A process can fork one last same-group child after the first KILL proof
    // but before the signal is delivered. Keep the *total* post-KILL budget
    // fixed and repeat only after a fresh receipt/census proof. If the old
    // group vanished and the numeric PGID was reused, `prove` rejects the new
    // membership and no signal crosses that identity boundary. A raw kill(0)
    // probe is only a short scheduling hint: with a non-zero budget, require
    // two typed post-KILL proofs even when the raw probe reports ESRCH. This
    // both exposes a late fork and gives a direct zombie leader a fresh,
    // explicitly authorized reap point.
    let kill_grace = grace_before_deadline(grace, signal_deadline);
    let kill_deadline = Instant::now() + kill_grace;
    let minimum_typed_proofs = usize::from(!kill_grace.is_zero()) * 2;
    let mut typed_proofs = 0usize;
    loop {
        let remaining = kill_deadline.saturating_duration_since(Instant::now());
        let probe_budget = remaining.min(Duration::from_millis(5));
        let raw_gone = match control.wait_until_gone(pgid, probe_budget) {
            Ok(gone) => gone,
            Err(wait_error) => {
                outcome.errors.push(format!(
                    "pgid {pgid} liveness failed after revalidated KILL: {wait_error:#}"
                ));
                break;
            }
        };
        if raw_gone && minimum_typed_proofs == 0 {
            outcome.terminated = true;
            break;
        }
        let proof = control.prove(pgid, owned);
        typed_proofs += 1;
        if termination_signal_budget_exhausted(&mut outcome, pgid, "repeated KILL", signal_deadline)
        {
            break;
        }
        match proof {
            Ok(GroupCredentialProof::AlreadyGone) => {
                if typed_proofs >= minimum_typed_proofs {
                    outcome.terminated = true;
                    outcome.zombie_awaiting_provider_reap = false;
                    break;
                }
            }
            Ok(GroupCredentialProof::OwnedZombieOnly) => {
                settle_owned_zombie_only(control, pgid, &mut outcome);
                if typed_proofs >= minimum_typed_proofs {
                    break;
                }
            }
            Ok(GroupCredentialProof::OwnedLive) => {
                // Preserve the zero-budget behavior: one immediate typed
                // refusal is enough, and no third signal is authorized after
                // its deadline. A non-zero convergence window may deliver
                // KILL only when the fresh OwnedLive proof itself completed
                // inside the fixed window. A/full/B may cross the deadline;
                // that proof remains read-only evidence and cannot authorize
                // a late signal.
                if grace.is_zero() {
                    outcome
                        .errors
                        .push(format!("pgid {pgid} remained live after revalidated KILL"));
                    break;
                }
                if Instant::now() >= kill_deadline {
                    if typed_proofs >= minimum_typed_proofs {
                        outcome
                            .errors
                            .push(format!("pgid {pgid} remained live after revalidated KILL"));
                        break;
                    }
                    continue;
                }
                if termination_signal_budget_exhausted(
                    &mut outcome,
                    pgid,
                    "repeated KILL",
                    signal_deadline,
                ) {
                    break;
                }
                outcome.kill_attempts += 1;
                if let Err(signal_error) = control.send(pgid, super::process::TerminationSignal::Kill)
                {
                    match control.wait_until_gone(pgid, Duration::ZERO) {
                        Ok(true) => outcome.terminated = true,
                        Ok(false) | Err(_) => outcome.errors.push(format!(
                            "pgid {pgid} repeated KILL failed before convergence: {signal_error:#}"
                        )),
                    }
                    break;
                }
                if typed_proofs >= minimum_typed_proofs && Instant::now() >= kill_deadline {
                    outcome
                        .errors
                        .push(format!("pgid {pgid} remained live after revalidated KILL"));
                    break;
                }
            }
            Err(errors) => {
                outcome.errors.extend(errors);
                outcome.errors.push(format!(
                    "pgid {pgid} identity changed or became ambiguous after KILL; repeated KILL refused"
                ));
                break;
            }
        }
        if typed_proofs >= minimum_typed_proofs && Instant::now() >= kill_deadline {
            if outcome.terminated || outcome.zombie_awaiting_provider_reap {
                break;
            }
            outcome
                .errors
                .push(format!("pgid {pgid} remained live after revalidated KILL"));
            break;
        }
    }
    outcome
}

fn extend_unique_strings(target: &mut Vec<String>, additions: Vec<String>) {
    for addition in additions {
        if !target.contains(&addition) {
            target.push(addition);
        }
    }
}

fn extend_unique_signals(
    target: &mut Vec<super::process::TerminationSignal>,
    additions: Vec<super::process::TerminationSignal>,
) {
    for addition in additions {
        if !target.contains(&addition) {
            target.push(addition);
        }
    }
}

fn validate_contained_live_member(
    observer: &mut ProcessObserver,
    member: &ManagedProcessTopology,
    containment_label: &str,
    ambiguity: &mut Vec<String>,
) -> bool {
    match observer.stable_credential(member.pid) {
        Ok(Some(now)) if credential_matches_topology(&now, member) => true,
        Ok(Some(_)) => {
            ambiguity.push(format!(
                "{containment_label} live pid {} full credential does not match topology",
                member.pid,
            ));
            false
        }
        Ok(None) => {
            ambiguity.push(format!(
                "{containment_label} live pid {} disappeared inside credential sandwich",
                member.pid,
            ));
            false
        }
        Err(error) => {
            ambiguity.push(format!(
                "{containment_label} live pid {} credential proof failed: {error:#}",
                member.pid,
            ));
            false
        }
    }
}

fn revalidate_group_credentials(
    observer: &mut ProcessObserver,
    pgid: u32,
    owned: &BTreeMap<u32, ManagedProcessCredential>,
) -> std::result::Result<GroupCredentialProof, Vec<String>> {
    let mut ambiguity = Vec::new();
    let before_census = match observer.topology_census() {
        Ok(census) => census,
        Err(error) => return Err(vec![format!("pgid {pgid} topology A failed: {error:#}")]),
    };
    let before = &before_census.topology;
    let member_pids = match exact_group_member_pids(pgid) {
        Ok(pids) => pids,
        Err(error) => {
            return Err(vec![format!(
                "pgid {pgid} exact census A failed: {error:#}"
            )])
        }
    };
    if member_pids.is_empty() {
        return match process_group_alive_exact(pgid) {
            Ok(false) => Ok(GroupCredentialProof::AlreadyGone),
            Ok(true) => Err(vec![format!(
                "pgid {pgid} is live but topology has no provable member"
            )]),
            Err(error) => Err(vec![format!(
                "pgid {pgid} liveness query failed: {error:#}"
            )]),
        };
    }
    let owned_group_leader = owned
        .get(&pgid)
        .filter(|leader| leader.pid == pgid && leader.pgid == pgid);
    let isolated_group_leader = owned_group_leader.filter(|leader| leader.sid == pgid);
    let owned_pids = owned.keys().copied().collect::<BTreeSet<_>>();
    let mut prevalidated_credentials = BTreeMap::new();
    let mut lineage_root_epochs = BTreeMap::new();
    let mut used_lineage_roots = BTreeSet::new();
    let isolated_session_anchor = isolated_group_leader.is_some_and(|leader| {
        member_pids.iter().any(|pid| {
            let Some(receipt) = owned.get(pid).filter(|receipt| receipt.sid == leader.sid) else {
                return false;
            };
            match before.get(pid) {
                Some(topology)
                    if topology.pgid == pgid
                        && (topology.sid == leader.sid
                            || (topology.zombie && topology.sid == 0))
                        && receipt_immutable_matches_topology(receipt, topology) =>
                {
                    if topology.zombie {
                        true
                    } else {
                        match observer.stable_credential(*pid) {
                            Ok(Some(now))
                                if credential_identity_matches(receipt, &now)
                                    && credential_identity_matches_topology(&now, topology) =>
                            {
                                prevalidated_credentials.insert(*pid, now);
                                true
                            }
                            _ => false,
                        }
                    }
                }
                Some(_) => false,
                None => before_census.opaque_zombies.get(pid).is_some_and(|zombie| {
                    zombie.pid == *pid
                        && zombie.pgid == pgid
                        && opaque_zombie_matches_receipt(&zombie, receipt)
                }),
            }
        })
    });
    let mut live_members = Vec::new();
    let mut zombie_epochs = BTreeMap::new();
    for pid in &member_pids {
        match exact_group_member_observation(&before_census, *pid, pgid) {
            Ok(ExactGroupMemberObservation::Live(topology)) => live_members.push(topology),
            Ok(ExactGroupMemberObservation::Zombie(proof)) => {
                // Full and opaque Darwin zombie rows have already collapsed
                // to one birth-bearing, no-SID epoch. It receives no signal
                // authority and cannot seed lineage; only a matching live-
                // captured receipt, or stable membership under a fresh
                // isolated leader, prevents it from vetoing convergence.
                if owned.get(pid).is_some_and(|receipt| {
                    owned_zombie_epoch_matches_receipt(receipt, &proof)
                        && (*pid != pgid || proof.birth_identity.is_some())
                }) || (*pid != pgid
                    && proof.pgid == pgid
                    && isolated_session_anchor
                    && isolated_group_leader.is_some_and(|leader| proof.uid == leader.uid))
                {
                    zombie_epochs.insert(*pid, proof);
                } else {
                    ambiguity.push(format!(
                        "pgid {pgid} zombie pid {pid} does not match an owned epoch or isolated leader"
                    ));
                }
            }
            Err(error) => ambiguity.push(format!(
                "pgid {pgid} exact member pid {pid} detail/binding failed: {error:#}"
            )),
        }
    }
    let mut unowned_live_members = Vec::new();
    for member in &live_members {
        let Some(expected) = owned.get(&member.pid) else {
            unowned_live_members.push(member);
            continue;
        };
        let current = prevalidated_credentials.remove(&member.pid).map_or_else(
            || observer.stable_credential(member.pid),
            |now| Ok(Some(now)),
        );
        match current {
            Ok(Some(now))
                if credential_identity_matches(expected, &now)
                    && credential_identity_matches_topology(&now, member) => {}
            Ok(Some(_)) => ambiguity.push(format!(
                "pid {} credential changed before signal (pgid/sid/uid/birth/executable)",
                member.pid
            )),
            Ok(None) => ambiguity.push(format!(
                "pid {} disappeared inside credential sandwich",
                member.pid
            )),
            Err(error) => ambiguity.push(format!(
                "pid {} credential revalidation failed: {error:#}",
                member.pid
            )),
        }
    }
    if let Some(leader) = owned_group_leader {
        if leader.sid == leader.pid {
            // Preserve the established session proof byte-for-byte in
            // strength: only a currently anchored pid=pgid=sid leader may
            // contain an otherwise-unowned member, and every member still
            // receives its own full-credential/topology sandwich.
            for member in unowned_live_members {
                let containment = classify_group_member(
                    member.pid,
                    member.observed_ppid,
                    member.pgid,
                    member.sid,
                    member.uid,
                    leader.pid,
                    leader.sid,
                    leader.uid,
                    &owned_pids,
                );
                if containment == MemberContainment::ContainedBySession && isolated_session_anchor {
                    validate_contained_live_member(observer, member, "isolated", &mut ambiguity);
                } else {
                    ambiguity.push(format!("pgid {pgid} contains unowned pid {}", member.pid));
                }
            }
        } else if !unowned_live_members.is_empty() {
            // A setpgid-only group has no session-wide containment theorem.
            // Build only a transient, credential-proven ancestry closure from
            // exact owned epochs. This covers two or more generations forked
            // inside a signal-delivery window without persisting new custody
            // authority. Merely migrating into the PGID cannot enter the
            // closure because it does not change PPID.
            for (pid, receipt) in owned {
                let Some(topology) = before.get(pid).filter(|topology| {
                    !topology.zombie && owned_lineage_root_matches(receipt, topology, None)
                }) else {
                    continue;
                };
                if observer
                    .stable_credential(*pid)
                    .ok()
                    .flatten()
                    .is_some_and(|current| credential_matches_topology(&current, topology))
                {
                    lineage_root_epochs.insert(*pid, topology.clone());
                }
            }
            let mut lineage_roots = lineage_root_epochs.keys().copied().collect::<BTreeSet<_>>();
            let mut lineage_origin = lineage_roots
                .iter()
                .copied()
                .map(|pid| (pid, pid))
                .collect::<BTreeMap<_, _>>();
            let mut pending = unowned_live_members;
            loop {
                let pending_len = pending.len();
                let mut next = Vec::new();
                for member in pending {
                    let containment = classify_group_member(
                        member.pid,
                        member.observed_ppid,
                        member.pgid,
                        member.sid,
                        member.uid,
                        leader.pid,
                        leader.sid,
                        leader.uid,
                        &lineage_roots,
                    );
                    if containment == MemberContainment::ContainedByLineage {
                        if validate_contained_live_member(
                            observer,
                            member,
                            "lineage-contained",
                            &mut ambiguity,
                        ) {
                            let Some(root) = lineage_origin.get(&member.observed_ppid).copied()
                            else {
                                ambiguity.push(format!(
                                    "pgid {pgid} lineage parent pid {} lacked a proven origin",
                                    member.observed_ppid
                                ));
                                continue;
                            };
                            used_lineage_roots.insert(root);
                            lineage_origin.insert(member.pid, root);
                            lineage_roots.insert(member.pid);
                        }
                    } else {
                        next.push(member);
                    }
                }
                if next.is_empty() {
                    break;
                }
                if next.len() == pending_len {
                    for member in next {
                        ambiguity.push(format!("pgid {pgid} contains unowned pid {}", member.pid));
                    }
                    break;
                }
                pending = next;
            }
        }
    } else {
        for member in unowned_live_members {
            ambiguity.push(format!("pgid {pgid} contains unowned pid {}", member.pid));
        }
    }
    let after_census = match observer.topology_census() {
        Ok(census) => Some(census),
        Err(error) => {
            ambiguity.push(format!("pgid {pgid} topology B failed: {error:#}"));
            None
        }
    };
    if let Some(after_census) = after_census {
        for root in &used_lineage_roots {
            if after_census.topology.get(root) != lineage_root_epochs.get(root) {
                ambiguity.push(format!(
                    "pgid {pgid} owned lineage root pid {root} changed in signal sandwich"
                ));
            }
        }
        let after_pids = match exact_group_member_pids(pgid) {
            Ok(pids) => pids,
            Err(error) => {
                ambiguity.push(format!("pgid {pgid} exact census B failed: {error:#}"));
                Vec::new()
            }
        };
        if member_pids != after_pids {
            ambiguity.push(format!("pgid {pgid} membership changed in signal sandwich"));
        }
        let mut after_live_members = Vec::new();
        let mut after_zombie_epochs = BTreeMap::new();
        for pid in &after_pids {
            match exact_group_member_observation(&after_census, *pid, pgid) {
                Ok(ExactGroupMemberObservation::Live(topology)) => {
                    after_live_members.push(topology);
                }
                Ok(ExactGroupMemberObservation::Zombie(proof)) => {
                    after_zombie_epochs.insert(*pid, proof);
                }
                Err(error) => ambiguity.push(format!(
                    "pgid {pgid} topology B exact member pid {pid} detail/binding failed: {error:#}"
                )),
            }
        }
        if zombie_epochs != after_zombie_epochs {
            ambiguity.push(format!(
                "pgid {pgid} canonical zombie epoch changed in signal sandwich"
            ));
        }
        if live_members != after_live_members {
            ambiguity.push(format!(
                "pgid {pgid} membership/topology changed in signal sandwich"
            ));
        }
    }
    if ambiguity.is_empty() {
        match process_group_alive_exact(pgid) {
            Ok(true)
                if !member_pids.is_empty()
                    && live_members.is_empty()
                    && zombie_epochs.len() == member_pids.len() =>
            {
                Ok(GroupCredentialProof::OwnedZombieOnly)
            }
            Ok(true) => Ok(GroupCredentialProof::OwnedLive),
            Ok(false) => Ok(GroupCredentialProof::AlreadyGone),
            Err(error) => Err(vec![format!(
                "pgid {pgid} liveness query failed: {error:#}"
            )]),
        }
    } else {
        Err(ambiguity)
    }
}

struct SupervisionDetails {
    outcome: WakeSupervisorOutcome,
    owned_helpers: usize,
    ambiguity: Vec<String>,
    exit_status: Option<i32>,
    termination_order: Vec<u32>,
    #[cfg(test)]
    signal_trace: Vec<(u32, super::process::TerminationSignal)>,
    error: Option<String>,
    observation_diagnostics: ManagedObservationDiagnostics,
    topology_snapshots: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    scheduled_topology_observations: usize,
    #[cfg(test)]
    containment_receipts_observed: usize,
    #[cfg(test)]
    pending_containment_groups_observed: usize,
    full_credential_inspections: usize,
    log_bytes_read: u64,
    last_frame_age_secs: Option<u64>,
    last_frame_summary: Option<String>,
    cancel_request: Option<ManagedWakeCancelRequest>,
    residual_child: Option<ManagedTreeOwner>,
}

struct ManagedTopologyCadence {
    next_due: Instant,
    warmup_remaining: u8,
}

/// Historical observation failures are diagnostics, never a stop disposition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedObservationDiagnostics {
    failures: u64,
    samples: Vec<String>,
}

impl ManagedObservationDiagnostics {
    fn is_empty(&self) -> bool { self.failures == 0 }
}

impl ManagedTopologyCadence {
    const WARMUP_OBSERVATIONS: u8 = 4;
    const WARMUP_INTERVAL: Duration = Duration::from_millis(250);
    const STEADY_INTERVAL: Duration = Duration::from_secs(5);

    fn new(now: Instant) -> Self {
        Self {
            next_due: now,
            warmup_remaining: Self::WARMUP_OBSERVATIONS,
        }
    }

    fn is_due(&self, now: Instant) -> bool {
        now >= self.next_due
    }

    fn record_completed_observation(&mut self, completed_at: Instant) {
        if self.warmup_remaining > 0 {
            self.warmup_remaining -= 1;
        }
        self.next_due = completed_at
            + if self.warmup_remaining > 0 {
                Self::WARMUP_INTERVAL
            } else {
                Self::STEADY_INTERVAL
            };
    }
}

struct ManagedLogTail {
    path: PathBuf,
    file: File,
    dev: u64,
    ino: u64,
    offset: u64,
    partial: Vec<u8>,
    discarding_oversized_record: bool,
    bytes_read: u64,
}

// A terminal record is a single JSON line, not an unbounded stream. Keep both
// retained partial state and each incremental read mechanically bounded. An
// oversized record is discarded through its LF before classification resumes,
// so no suffix of attacker-controlled bytes can become a synthetic record.
const MAX_MANAGED_LOG_RECORD_BYTES: usize = 1024 * 1024;
const MAX_MANAGED_LOG_READ_PER_POLL: usize = 256 * 1024;
const MAX_MANAGED_LAST_FRAME_SUMMARY_CHARS: usize = 256;

fn managed_last_frame_summary(program: &str, line: &str) -> Option<String> {
    // Provider frames are JSONL. Harness noise and other complete non-JSON
    // lines remain part of logBytesRead but cannot displace the last frame.
    serde_json::from_str::<serde_json::Value>(line).ok()?;
    let summary =
        crate::activity::parse_activity_line(program, "<managed-wake-log>", 0, line).summary;
    Some(
        summary
            .chars()
            .take(MAX_MANAGED_LAST_FRAME_SUMMARY_CHARS)
            .collect(),
    )
}

impl ManagedLogTail {
    fn open(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("stat managed wake log failed: {}", path.display()))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            bail!("managed wake log must be a regular non-symlink");
        }
        let file = File::open(path)
            .with_context(|| format!("open managed wake log failed: {}", path.display()))?;
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            bail!("managed wake log changed while opening");
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            dev: opened.dev(),
            ino: opened.ino(),
            offset: 0,
            partial: Vec::new(),
            discarding_oversized_record: false,
            bytes_read: 0,
        })
    }

    fn read_complete_lines(&mut self) -> Result<Vec<String>> {
        let path_metadata = fs::symlink_metadata(&self.path)
            .with_context(|| format!("restat managed wake log failed: {}", self.path.display()))?;
        if !path_metadata.file_type().is_file()
            || path_metadata.file_type().is_symlink()
            || path_metadata.dev() != self.dev
            || path_metadata.ino() != self.ino
        {
            bail!("managed wake log was replaced during supervision");
        }
        let metadata = self.file.metadata()?;
        if metadata.len() < self.offset {
            bail!("managed wake log shrank during supervision");
        }
        let available = metadata.len() - self.offset;
        let delta = available.min(MAX_MANAGED_LOG_READ_PER_POLL as u64);
        let mut appended = vec![0u8; delta as usize];
        self.file
            .read_exact(&mut appended)
            .context("read managed wake log increment failed")?;
        if appended.len() as u64 != delta {
            bail!("managed wake log short incremental read");
        }
        self.offset = self.offset.saturating_add(delta);
        self.bytes_read = self.bytes_read.saturating_add(delta);
        let mut lines = Vec::new();
        let push_line = |mut line: Vec<u8>, lines: &mut Vec<String>| {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Ok(line) = String::from_utf8(line) {
                lines.push(line);
            }
        };
        // Scan every appended byte exactly once. While discarding an
        // oversized record, no bytes are retained and only LF can restore the
        // normal parser state. The bytes after that LF are a new record even
        // when they arrived in the same kernel read.
        for byte in appended {
            if self.discarding_oversized_record {
                if byte == b'\n' {
                    self.discarding_oversized_record = false;
                }
                continue;
            }
            if byte == b'\n' {
                push_line(std::mem::take(&mut self.partial), &mut lines);
            } else if self.partial.len() < MAX_MANAGED_LOG_RECORD_BYTES {
                self.partial.push(byte);
            } else {
                self.partial.clear();
                self.discarding_oversized_record = true;
            }
        }
        Ok(lines)
    }
}

struct ManagedTreeOwner {
    child: Option<Child>,
    child_status: Option<ExitStatus>,
    provider_pid: u32,
    provider_pgid: u32,
    isolated_sid: Option<u32>,
    baseline: BTreeMap<u32, ManagedProcessTopology>,
    owned: BTreeMap<u32, ManagedProcessCredential>,
    /// Stable A/B zombie rows from the current provider SID. These receipts
    /// authorize only `await provider reap`, never a signal or lineage root.
    zombie_containment: BTreeMap<u32, ZombieContainmentReceipt>,
    /// Group-level containment proofs already accepted while the provider
    /// anchor was live. They remain explicit discharge obligations after the
    /// provider is reaped, even if ordinary topology stops showing zombies.
    pending_zombie_containment: BTreeMap<u32, BTreeMap<u32, ZombieContainmentReceipt>>,
    #[cfg(test)]
    containment_receipts_observed: usize,
    #[cfg(test)]
    pending_containment_groups_observed: usize,
    #[cfg(test)]
    residual_signal_trace: Vec<(u32, super::process::TerminationSignal)>,
    ever_owned_helpers: BTreeSet<u32>,
    observer: ProcessObserver,
    provider_exit_retry_at: Option<Instant>,
    residual_convergence_started_at: Option<Instant>,
    residual_convergence_giveup: Option<Vec<String>>,
    #[cfg(test)]
    residual_convergence_budget_override: Option<Duration>,
}

#[derive(Debug, Default)]
struct HelperGroupPartition {
    live: Vec<u32>,
    owned_zombie_awaiting_provider_reap: BTreeSet<u32>,
    containment_zombie_awaiting_provider_reap: BTreeSet<u32>,
}

impl HelperGroupPartition {
    fn all_zombies(&self) -> BTreeSet<u32> {
        self.owned_zombie_awaiting_provider_reap
            .union(&self.containment_zombie_awaiting_provider_reap)
            .copied()
            .collect()
    }
}

#[derive(Debug, Default)]
struct ResidualCleanupEvidence {
    signals: Vec<super::process::TerminationSignal>,
    termination_order: Vec<u32>,
    #[cfg(test)]
    signal_trace: Vec<(u32, super::process::TerminationSignal)>,
    exit_status: Option<i32>,
    owned_helpers: usize,
    metrics: ProcessObserverMetrics,
    #[cfg(test)]
    containment_receipts_observed: usize,
    #[cfg(test)]
    pending_containment_groups_observed: usize,
}

#[derive(Debug)]
struct ResidualConvergenceFailure {
    evidence: ResidualCleanupEvidence,
    messages: Vec<String>,
}

impl ResidualConvergenceFailure {
    fn message(&self) -> String {
        self.messages.join("; ")
    }
}

impl ManagedTreeOwner {
    fn new(
        child: Child,
        baseline: Vec<ManagedProcessTopology>,
        provider_credential: Option<ManagedProcessCredential>,
        isolated_session: bool,
        observer: ProcessObserver,
    ) -> Self {
        let provider_pid = child.id();
        let mut owned = BTreeMap::new();
        if let Some(provider) = provider_credential {
            owned.insert(provider.pid, provider);
        }
        Self {
            child: Some(child),
            child_status: None,
            provider_pid,
            provider_pgid: provider_pid,
            isolated_sid: isolated_session.then_some(provider_pid),
            baseline: baseline
                .into_iter()
                .map(|topology| (topology.pid, topology))
                .collect(),
            owned,
            zombie_containment: BTreeMap::new(),
            pending_zombie_containment: BTreeMap::new(),
            #[cfg(test)]
            containment_receipts_observed: 0,
            #[cfg(test)]
            pending_containment_groups_observed: 0,
            #[cfg(test)]
            residual_signal_trace: Vec::new(),
            ever_owned_helpers: BTreeSet::new(),
            observer,
            provider_exit_retry_at: None,
            residual_convergence_started_at: None,
            residual_convergence_giveup: None,
            #[cfg(test)]
            residual_convergence_budget_override: None,
        }
    }

    fn observe(&mut self) -> Result<BTreeMap<u32, ManagedProcessTopology>> {
        let snapshot = observe_owned_helpers(
            &mut self.observer,
            self.provider_pid,
            self.isolated_sid,
            &self.baseline,
            &mut self.owned,
            &mut self.zombie_containment,
        )?;
        let current_owned_helpers = self
            .owned
            .keys()
            .copied()
            .filter(|pid| *pid != self.provider_pid)
            .collect::<BTreeSet<_>>();
        self.ever_owned_helpers
            .extend(current_owned_helpers.iter().copied());
        self.observer.notify_owned_promotion(&current_owned_helpers);
        #[cfg(test)]
        {
            self.containment_receipts_observed = self
                .containment_receipts_observed
                .max(self.zombie_containment.len());
        }
        Ok(snapshot)
    }

    fn provider_exit_visible_or_hold(&mut self) -> bool {
        if self.provider_exit_retry_at.is_some_and(|next| Instant::now() < next) {
            return false;
        }
        match self.provider_exit_visible_without_reap() {
            Ok(visible) => { self.provider_exit_retry_at = None; visible }
            Err(error) => {
                self.observer.record_observation_failure("provider exit proof", &error);
                self.provider_exit_retry_at = Some(Instant::now() + ManagedTopologyCadence::STEADY_INTERVAL);
                false
            }
        }
    }

    fn ensure_provider_owned(&mut self) -> Result<()> {
        if self.owned.contains_key(&self.provider_pid) {
            return Ok(());
        }
        let credential = capture_provider_credential(&mut self.observer, self.provider_pid)?;
        self.owned.insert(self.provider_pid, credential);
        Ok(())
    }

    fn ensure_provider_cleanup_owned(&mut self) -> Result<()> {
        if self.owned.contains_key(&self.provider_pid) {
            return Ok(());
        }
        if self.isolated_sid.is_some() {
            if let Some(epoch) = capture_isolated_zombie_provider_epoch(self.provider_pid)? {
                self.owned.insert(self.provider_pid, epoch);
                return Ok(());
            }
        }
        self.ensure_provider_owned()
    }

    fn poll_child(&mut self) -> Result<()> {
        if self.child_status.is_none() {
            let child = self
                .child
                .as_mut()
                .context("managed child owner is empty")?;
            self.child_status = child
                .try_wait()
                .context("try_wait managed provider failed")?;
        }
        Ok(())
    }

    fn provider_exit_visible_without_reap(&self) -> Result<bool> {
        if self.child_status.is_some() {
            return Ok(true);
        }
        if let Some(topology) = inspect_topology(self.provider_pid)? {
            if !topology.zombie {
                return Ok(false);
            }
            if let Some(receipt) = self.owned.get(&self.provider_pid) {
                if !receipt_immutable_matches_topology(receipt, &topology)
                    || topology.pgid != receipt.pgid
                    || !(topology.sid == receipt.sid || topology.sid == 0)
                {
                    bail!("provider zombie topology does not match its live-captured receipt");
                }
            }
            return Ok(true);
        }
        match inspect_opaque_zombie(self.provider_pid)? {
            Some(zombie) => {
                if let Some(receipt) = self.owned.get(&self.provider_pid) {
                    if !opaque_zombie_matches_receipt(&zombie, receipt) {
                        bail!("opaque provider zombie does not match its live-captured receipt");
                    }
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// A provider that exits before any full receipt can be captured may be
    /// reaped only when two exact censuses prove its group contains that one
    /// explicit zombie and nothing else. This is a reap authority, never a
    /// signal authority.
    fn reap_uncredentialed_zombie_only_provider(&mut self) -> Result<bool> {
        if self.owned.contains_key(&self.provider_pid) || self.child_status.is_some() {
            return Ok(false);
        }
        let before_members = exact_group_member_pids(self.provider_pgid)?;
        if before_members != [self.provider_pid] {
            return Ok(false);
        }
        let Some(before) = inspect_opaque_zombie(self.provider_pid)? else {
            return Ok(false);
        };
        if before.pgid != self.provider_pgid {
            return Ok(false);
        }
        let after_members = exact_group_member_pids(self.provider_pgid)?;
        let Some(after) = inspect_opaque_zombie(self.provider_pid)? else {
            return Ok(false);
        };
        if before_members != after_members || before != after {
            return Ok(false);
        }
        self.poll_child()?;
        if self.child_status.is_none() {
            bail!("explicit zombie provider was not waitable through its Child handle");
        }
        if process_group_alive_exact(self.provider_pgid)? {
            bail!("provider group remained live after exact singleton zombie reap");
        }
        Ok(true)
    }

    fn helper_groups(&self) -> BTreeSet<u32> {
        self.owned
            .values()
            .map(|credential| credential.pgid)
            .filter(|pgid| *pgid != self.provider_pgid)
            .collect()
    }

    fn child_exit_code(&self) -> Option<i32> {
        self.child_status.as_ref().and_then(|status| {
            status
                .code()
                .or_else(|| status.signal().map(|signal| -signal))
        })
    }

    fn release_reaped_child(&mut self) {
        if self.child_status.is_some() {
            self.child.take();
        }
    }

    fn take_child(&mut self) -> Option<Child> {
        self.child.take()
    }

    /// Prove a same-provider-SID helper group is composed only of direct,
    /// unowned zombies whose exact epochs were captured by an earlier
    /// A/full/B observation. This disposition is containment-only: callers
    /// may wait for provider reap, but must never signal this PGID.
    fn prove_unowned_zombie_containment_group(&mut self, pgid: u32) -> Result<bool> {
        if self.isolated_sid.is_none() {
            return Ok(false);
        }
        if pgid == self.provider_pgid {
            return Ok(false);
        }
        let before_members = exact_group_member_pids(pgid)?;
        if before_members.is_empty() {
            return Ok(false);
        }
        let before_census = self.observer.topology_census()?;
        let after_census = self.observer.topology_census()?;
        let after_members = exact_group_member_pids(pgid)?;

        if before_members != after_members {
            return Ok(false);
        }

        let mut accepted_members = BTreeMap::new();
        for pid in &before_members {
            let Some(receipt) = self.zombie_containment.get(pid) else {
                return Ok(false);
            };
            if receipt.pid != *pid || receipt.birth_identity.is_empty() {
                return Ok(false);
            }
            // Bind both exact-list rows to the target PGID before comparing
            // epochs. Historical PPID/UID/PGID/SID structure may legitimately
            // have changed after capture; same-birth stable A/B still creates
            // only a no-signal custody obligation.
            if exact_group_member_observation(&before_census, *pid, pgid).is_err()
                || exact_group_member_observation(&after_census, *pid, pgid).is_err()
            {
                return Ok(false);
            }
            let Some(first) = normalized_zombie_epoch(&before_census, *pid)? else {
                return Ok(false);
            };
            let Some(second) = normalized_zombie_epoch(&after_census, *pid)? else {
                return Ok(false);
            };
            if !stable_pending_containment_epoch(receipt, &first, &second, pgid) {
                return Ok(false);
            }
            accepted_members.insert(*pid, receipt.clone());
        }
        self.pending_zombie_containment
            .insert(pgid, accepted_members);
        #[cfg(test)]
        {
            self.pending_containment_groups_observed = self
                .pending_containment_groups_observed
                .max(self.pending_zombie_containment.len());
        }
        Ok(true)
    }

    /// Retire only the exact zombie epochs that have actually disappeared.
    /// A reused numeric PGID/PID never becomes a signal target and does not
    /// keep an old obligation alive once every recorded birth epoch is gone.
    fn refresh_pending_zombie_containment(&mut self) -> Result<()> {
        let mut discharged = Vec::new();
        for (pgid, members) in &self.pending_zombie_containment {
            let exact_before = exact_group_member_pids(*pgid)?;
            if exact_before.is_empty() && process_group_alive_exact(*pgid)? {
                bail!("pending zombie pgid {pgid} is live without an exact member census");
            }
            let before_census = self.observer.topology_census()?;
            let after_census = self.observer.topology_census()?;
            let exact_after = exact_group_member_pids(*pgid)?;
            if exact_before != exact_after {
                bail!("pending zombie pgid {pgid} membership changed during discharge proof");
            }
            let mut old_epoch_present_in_group = false;
            for (pid, receipt) in members {
                let listed_before = exact_before.contains(pid);
                let listed_after = exact_after.contains(pid);
                if listed_before != listed_after {
                    bail!("pending zombie pid {pid} moved during exact A/B discharge proof");
                }
                let first = classify_custody_epoch(receipt, &before_census)?;
                let second = classify_custody_epoch(receipt, &after_census)?;
                if listed_before {
                    exact_group_member_observation(&before_census, *pid, *pgid)
                        .with_context(|| format!("pending pid {pid} exact row A"))?;
                    exact_group_member_observation(&after_census, *pid, *pgid)
                        .with_context(|| format!("pending pid {pid} exact row B"))?;
                }
                match (&first, &second) {
                    (CustodyEpochState::PresentExact, CustodyEpochState::PresentExact)
                    | (
                        CustodyEpochState::PresentStructuralConflict,
                        CustodyEpochState::PresentStructuralConflict,
                    ) if listed_before => old_epoch_present_in_group = true,
                    (CustodyEpochState::Absent, CustodyEpochState::Absent) => {}
                    (CustodyEpochState::Replaced(first), CustodyEpochState::Replaced(second))
                        if first == second => {}
                    // A same-birth zombie that stably moved to another PGID
                    // retires only this pending-group projection. The durable
                    // receipt remains in zombie_containment and will late-
                    // promote under its freshly proven current group.
                    (CustodyEpochState::PresentExact, CustodyEpochState::PresentExact)
                    | (
                        CustodyEpochState::PresentStructuralConflict,
                        CustodyEpochState::PresentStructuralConflict,
                    ) if !listed_before => {}
                    (CustodyEpochState::MissingDetail, _)
                    | (_, CustodyEpochState::MissingDetail) => {
                        bail!("pending zombie pid {pid} is declared without exact detail")
                    }
                    (CustodyEpochState::PresentNonZombieConflict, _)
                    | (_, CustodyEpochState::PresentNonZombieConflict) => {
                        bail!("pending zombie pid {pid} has the same birth but a live status")
                    }
                    _ => bail!("pending zombie pid {pid} epoch changed during A/B discharge"),
                }
            }
            if !old_epoch_present_in_group {
                discharged.push(*pgid);
            }
        }
        for pgid in discharged {
            self.pending_zombie_containment.remove(&pgid);
        }
        Ok(())
    }

    fn live_isolated_helper_groups(&mut self) -> Result<Vec<u32>> {
        // Provider-session discovery catches not-yet-promoted descendants;
        // receipted helper PGIDs catch already-owned processes that later
        // escaped via setsid/setpgid/reparent. Neither set may be dropped from
        // the provider-before-helper fence.
        self.refresh_pending_zombie_containment()?;
        let before_census = self.observer.topology_census()?;
        let has_owned_helpers = self.owned.keys().any(|pid| *pid != self.provider_pid);
        let after_census = if !has_owned_helpers && self.zombie_containment.is_empty() {
            None
        } else {
            Some(self.observer.topology_census()?)
        };
        // A final partition can race immediately after the preceding observe.
        // Resolve every still-owned epoch from its own adjacent fresh A/B pair;
        // compute all decisions before mutating the map. Never reuse this pair
        // across a signal boundary or a later convergence iteration.
        let mut retired_owned = Vec::new();
        if let Some(after) = after_census.as_ref() {
            for (pid, receipt) in self
                .owned
                .iter()
                .filter(|(pid, _)| **pid != self.provider_pid)
            {
                if owned_epoch_retired_by_stable_pair(receipt, &before_census, after)? {
                    retired_owned.push(*pid);
                }
            }
        }
        for pid in retired_owned {
            self.owned.remove(&pid);
        }
        let census = after_census.as_ref().unwrap_or(&before_census);
        let snapshot = &census.topology;
        let mut groups = BTreeSet::new();
        groups.extend(self.pending_zombie_containment.keys().copied());
        if let Some(sid) = self.isolated_sid {
            groups.extend(
                snapshot
                    .values()
                    .filter(|topology| topology.sid == sid && topology.pgid != self.provider_pgid)
                    .map(|topology| topology.pgid),
            );
        }
        for (pid, receipt) in &self.owned {
            if *pid == self.provider_pid {
                continue;
            }
            // Track the current group of the same kernel epoch even when
            // UID/SID/PGID authority drifted. The old receipt is deliberately
            // left unchanged: exact group revalidation must refuse every
            // signal until an independently proven credential refresh exists.
            // A missing/replacement row cannot be ignored here; only the
            // stable A/B retirement in observe_owned_helpers removes `owned`.
            if let Some(pgid) = current_owned_epoch_group(receipt, &census, self.provider_pgid)? {
                groups.insert(pgid);
            }
        }
        let mut retired_containment = Vec::new();
        for (pid, receipt) in &self.zombie_containment {
            if let Some(after) = after_census.as_ref() {
                if custody_epoch_retired_by_stable_pair(receipt, &before_census, after)? {
                    retired_containment.push(*pid);
                    continue;
                }
            }
            match classify_custody_epoch(receipt, &census)? {
                CustodyEpochState::PresentExact | CustodyEpochState::PresentStructuralConflict => {
                    let current = normalized_zombie_epoch(&census, *pid)?
                        .context("present containment epoch lacks zombie detail")?;
                    if current.pgid != self.provider_pgid {
                        groups.insert(current.pgid);
                    }
                }
                CustodyEpochState::PresentNonZombieConflict => {
                    bail!("containment pid {pid} has the same birth but is no longer zombie")
                }
                CustodyEpochState::MissingDetail => {
                    bail!("containment pid {pid} is declared without exact detail")
                }
                CustodyEpochState::Replaced(_) | CustodyEpochState::Absent => {
                    bail!("containment pid {pid} awaits a stable A/B retirement proof")
                }
            }
        }
        for pid in retired_containment {
            self.zombie_containment.remove(&pid);
        }
        groups
            .into_iter()
            .filter_map(|pgid| match process_group_alive_exact(pgid) {
                Ok(true) => Some(Ok(pgid)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Partition a fresh isolated-session census into signalable live groups
    /// and exact zombie-only groups. Zombie-only helpers are not "gone", but
    /// they cannot consume a signal; they are held as an explicit dependency
    /// that must disappear after the direct provider is terminated and reaped.
    fn partition_live_helper_groups(&mut self) -> Result<HelperGroupPartition> {
        let groups = self.live_isolated_helper_groups()?;
        let mut partition = HelperGroupPartition::default();
        for pgid in groups {
            if self.pending_zombie_containment.contains_key(&pgid) {
                partition
                    .containment_zombie_awaiting_provider_reap
                    .insert(pgid);
                continue;
            }
            let proof = {
                let owned = &self.owned;
                revalidate_group_credentials(&mut self.observer, pgid, owned)
            };
            match proof {
                Ok(GroupCredentialProof::AlreadyGone) => {}
                Ok(GroupCredentialProof::OwnedLive) => partition.live.push(pgid),
                Ok(GroupCredentialProof::OwnedZombieOnly) => {
                    partition.owned_zombie_awaiting_provider_reap.insert(pgid);
                }
                Err(errors) => {
                    if self.prove_unowned_zombie_containment_group(pgid)? {
                        partition
                            .containment_zombie_awaiting_provider_reap
                            .insert(pgid);
                    } else {
                        bail!(
                            "helper pgid {pgid} exact partition proof refused: {}",
                            errors.join("; ")
                        );
                    }
                }
            }
        }
        Ok(partition)
    }

    fn residual_convergence_budget(&self) -> Duration {
        #[cfg(test)]
        if let Some(override_budget) = self.residual_convergence_budget_override {
            return override_budget;
        }
        RESIDUAL_CONVERGENCE_BUDGET
    }

    fn residual_convergence_candidate_groups(&self) -> BTreeSet<u32> {
        let mut groups = self.helper_groups();
        groups.insert(self.provider_pgid);
        groups.extend(self.pending_zombie_containment.keys().copied());
        groups.extend(self.zombie_containment.values().map(|receipt| receipt.pgid));
        if let Some(sid) = self.isolated_sid {
            // Diagnostics must retain groups discovered from the live session
            // even when their ownership proof is the very ambiguity that
            // prevents promotion into `owned`. A failed diagnostic snapshot
            // cannot erase groups already retained in `latest` by the caller.
            if let Ok(snapshot) = process_topology_snapshot() {
                groups.extend(
                    snapshot
                        .values()
                        .filter(|topology| topology.sid == sid)
                        .map(|topology| topology.pgid),
                );
            }
        }
        groups.retain(|pgid| *pgid != 0);
        groups
    }

    /// Retain the latest exact membership for every unresolved group. A
    /// transient final census failure cannot erase the PID that caused the
    /// preceding fail-closed refusal.
    fn refresh_residual_unresolved_members(&self, latest: &mut BTreeMap<u32, BTreeSet<u32>>) {
        let mut groups = self.residual_convergence_candidate_groups();
        groups.extend(latest.keys().copied());
        for pgid in groups {
            match exact_group_member_pids(pgid) {
                Ok(members) if !members.is_empty() => {
                    latest.insert(pgid, members.into_iter().collect());
                }
                Ok(_) if process_group_alive_exact(pgid).is_ok_and(|alive| !alive) => {
                    latest.remove(&pgid);
                }
                Ok(_) | Err(_) => {
                    let fallback = self
                        .owned
                        .values()
                        .filter(|receipt| receipt.pgid == pgid)
                        .map(|receipt| receipt.pid)
                        .chain(
                            (pgid == self.provider_pgid && self.child_status.is_none())
                                .then_some(self.provider_pid),
                        )
                        .collect::<BTreeSet<_>>();
                    if !fallback.is_empty() {
                        latest.entry(pgid).or_insert(fallback);
                    }
                }
            }
        }
    }

    fn residual_convergence_failure(
        &mut self,
        signals: Vec<super::process::TerminationSignal>,
        termination_order: Vec<u32>,
        latest: &mut BTreeMap<u32, BTreeSet<u32>>,
    ) -> ResidualConvergenceFailure {
        // Latch before the final diagnostic census. If diagnostics panic or
        // race, Drop must still never restart convergence and signal again.
        self.residual_convergence_giveup = Some(Vec::new());
        self.refresh_residual_unresolved_members(latest);
        let messages = if latest.is_empty() {
            vec![convergence_giveup_message(self.provider_pgid, &[])]
        } else {
            latest
                .iter()
                .map(|(pgid, members)| {
                    convergence_giveup_message(*pgid, &members.iter().copied().collect::<Vec<_>>())
                })
                .collect::<Vec<_>>()
        };
        self.residual_convergence_giveup = Some(messages.clone());
        ResidualConvergenceFailure {
            evidence: self.residual_cleanup_evidence(signals, termination_order),
            messages,
        }
    }

    /// Fail-closed RAII custody for every path between provider spawn and the
    /// normal supervisor result. Each pass rediscovers the isolated session
    /// with the same light/full/light proof as the steady-state observer, then
    /// terminates proven live helper groups before the provider group. An
    /// ambiguous receipt never degrades into a positive-PID leader kill or a
    /// naked `Child` drop. Retries remain fail-closed, but they now have an
    /// explicit budget and return named unresolved membership instead of
    /// spinning forever.
    fn residual_cleanup_evidence(
        &self,
        signals: Vec<super::process::TerminationSignal>,
        termination_order: Vec<u32>,
    ) -> ResidualCleanupEvidence {
        ResidualCleanupEvidence {
            signals,
            termination_order,
            #[cfg(test)]
            signal_trace: self.residual_signal_trace.clone(),
            exit_status: self.child_exit_code(),
            owned_helpers: self.ever_owned_helpers.len(),
            metrics: self.observer.metrics,
            #[cfg(test)]
            containment_receipts_observed: self.containment_receipts_observed,
            #[cfg(test)]
            pending_containment_groups_observed: self.pending_containment_groups_observed,
        }
    }

    fn converge_residual_tree(
        &mut self,
    ) -> std::result::Result<ResidualCleanupEvidence, ResidualConvergenceFailure> {
        let mut signals = Vec::new();
        let mut termination_order = Vec::new();
        if self.child.is_none() {
            return Ok(self.residual_cleanup_evidence(signals, termination_order));
        }
        if let Some(messages) = self.residual_convergence_giveup.clone() {
            return Err(ResidualConvergenceFailure {
                evidence: self.residual_cleanup_evidence(signals, termination_order),
                messages,
            });
        }
        let budget = self.residual_convergence_budget();
        let started_at = *self
            .residual_convergence_started_at
            .get_or_insert_with(Instant::now);
        let signal_deadline = started_at + budget;
        let mut latest_unresolved = BTreeMap::new();
        let mut last_notice: Option<String> = None;
        loop {
            self.refresh_residual_unresolved_members(&mut latest_unresolved);
            let elapsed = started_at.elapsed();
            let budget_exhausted = if budget == RESIDUAL_CONVERGENCE_BUDGET {
                convergence_budget_exhausted(elapsed)
            } else {
                elapsed >= budget
            };
            if budget_exhausted {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            let mut defer = |message: String| {
                if last_notice.as_deref() != Some(message.as_str()) {
                    eprintln!("orch: {message}");
                    last_notice = Some(message);
                }
                let remaining = signal_deadline.saturating_duration_since(Instant::now());
                if !remaining.is_zero() {
                    std::thread::sleep(Duration::from_millis(250).min(remaining));
                }
            };

            if self.child_status.is_none() && !self.owned.contains_key(&self.provider_pid) {
                if let Err(error) = self.ensure_provider_cleanup_owned() {
                    defer(format!(
                        "residual provider credential ambiguous; signal deferred: {error:#}"
                    ));
                    continue;
                }
            }
            if let Err(error) = self.observe() {
                defer(format!("residual tree observation failed: {error:#}"));
                continue;
            }
            if Instant::now() >= signal_deadline {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            let partition = match self.partition_live_helper_groups() {
                Ok(groups) => groups,
                Err(error) => {
                    defer(format!(
                        "residual helper proof ambiguous; provider signal deferred: {error:#}"
                    ));
                    continue;
                }
            };
            if Instant::now() >= signal_deadline {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            for pgid in &partition.owned_zombie_awaiting_provider_reap {
                if !termination_order.contains(&pgid) {
                    termination_order.push(*pgid);
                }
            }
            let mut helper_refused = None;
            for pgid in partition.live {
                let termination = {
                    let owned = &self.owned;
                    let mut control = SystemGroupControl {
                        observer: &mut self.observer,
                        direct_child: None,
                    };
                    terminate_revalidated_group_until(
                        &mut control,
                        pgid,
                        owned,
                        Duration::from_millis(100),
                        Some(signal_deadline),
                    )
                };
                let convergence_budget_exhausted = termination.convergence_budget_exhausted;
                #[cfg(test)]
                self.residual_signal_trace.extend(
                    termination
                        .signals
                        .iter()
                        .copied()
                        .map(|signal| (pgid, signal)),
                );
                if (!termination.signals.is_empty() || termination.zombie_awaiting_provider_reap)
                    && !termination_order.contains(&pgid)
                {
                    termination_order.push(pgid);
                }
                extend_unique_signals(&mut signals, termination.signals);
                if convergence_budget_exhausted || Instant::now() >= signal_deadline {
                    return Err(self.residual_convergence_failure(
                        signals,
                        termination_order,
                        &mut latest_unresolved,
                    ));
                }
                if !termination.errors.is_empty() {
                    helper_refused = Some(format!(
                        "residual helper pgid {pgid} proof refused: {}",
                        termination.errors.join("; ")
                    ));
                    break;
                }
            }
            if let Some(message) = helper_refused {
                defer(message);
                continue;
            }

            // A second fresh partition is the provider-before-helper fence.
            // Zombie-only groups may cross it because they have no executable
            // image; their exact PGIDs must disappear after provider reap.
            let remaining = match self.partition_live_helper_groups() {
                Ok(groups) => groups,
                Err(error) => {
                    defer(format!(
                        "residual pre-provider helper proof failed: {error:#}"
                    ));
                    continue;
                }
            };
            if Instant::now() >= signal_deadline {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            for pgid in &remaining.owned_zombie_awaiting_provider_reap {
                if !termination_order.contains(&pgid) {
                    termination_order.push(*pgid);
                }
            }
            if !remaining.live.is_empty() {
                defer(format!(
                    "residual live helpers remain; provider signal deferred: {:?}",
                    remaining.live
                ));
                continue;
            }
            if self.child_status.is_none() && !self.owned.contains_key(&self.provider_pid) {
                if let Err(error) = self.ensure_provider_cleanup_owned() {
                    defer(format!(
                        "residual provider credential ambiguous; signal deferred: {error:#}"
                    ));
                    continue;
                }
            }
            let provider_reap_authorized = match process_group_alive_exact(self.provider_pgid) {
                Ok(true) => {
                    let termination = {
                        let owned = &self.owned;
                        let mut control = SystemGroupControl {
                            observer: &mut self.observer,
                            direct_child: self
                                .child
                                .as_mut()
                                .map(|child| (child, &mut self.child_status)),
                        };
                        terminate_revalidated_group_until(
                            &mut control,
                            self.provider_pgid,
                            owned,
                            Duration::from_millis(250),
                            Some(signal_deadline),
                        )
                    };
                    let convergence_budget_exhausted = termination.convergence_budget_exhausted;
                    #[cfg(test)]
                    self.residual_signal_trace.extend(
                        termination
                            .signals
                            .iter()
                            .copied()
                            .map(|signal| (self.provider_pgid, signal)),
                    );
                    if (!termination.signals.is_empty()
                        || termination.zombie_awaiting_provider_reap)
                        && !termination_order.contains(&self.provider_pgid)
                    {
                        termination_order.push(self.provider_pgid);
                    }
                    extend_unique_signals(&mut signals, termination.signals);
                    if convergence_budget_exhausted || Instant::now() >= signal_deadline {
                        return Err(self.residual_convergence_failure(
                            signals,
                            termination_order,
                            &mut latest_unresolved,
                        ));
                    }
                    if !termination.errors.is_empty() {
                        defer(format!(
                            "residual provider group proof refused: {}",
                            termination.errors.join("; ")
                        ));
                        continue;
                    }
                    termination.terminated || termination.zombie_awaiting_provider_reap
                }
                Ok(false) => true,
                Err(error) => {
                    defer(format!("residual provider group probe failed: {error:#}"));
                    continue;
                }
            };
            if !provider_reap_authorized {
                defer("residual provider group still has signalable members".to_string());
                continue;
            }
            if let Err(error) = self.poll_child() {
                defer(format!("residual provider reap failed: {error:#}"));
                continue;
            }
            if Instant::now() >= signal_deadline {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            if self.child_status.is_none() {
                defer("residual provider remains under supervisor custody".to_string());
                continue;
            }
            let final_partition = match self.partition_live_helper_groups() {
                Ok(groups) => groups,
                Err(error) => {
                    defer(format!("residual final helper proof failed: {error:#}"));
                    continue;
                }
            };
            if Instant::now() >= signal_deadline {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            let provider_gone =
                process_group_alive_exact(self.provider_pgid).is_ok_and(|alive| !alive);
            let final_zombies = final_partition.all_zombies();
            if !provider_gone || !final_partition.live.is_empty() || !final_zombies.is_empty() {
                defer(format!(
                    "residual tree awaiting kernel reap: providerGone={provider_gone} live={:?} zombies={final_zombies:?}",
                    final_partition.live
                ));
                continue;
            }
            if Instant::now() >= signal_deadline {
                return Err(self.residual_convergence_failure(
                    signals,
                    termination_order,
                    &mut latest_unresolved,
                ));
            }
            self.release_reaped_child();
            return Ok(self.residual_cleanup_evidence(signals, termination_order));
        }
    }
}

impl Drop for ManagedTreeOwner {
    fn drop(&mut self) {
        if self.child.is_none() || self.residual_convergence_giveup.is_some() {
            return;
        }
        if let Err(failure) = self.converge_residual_tree() {
            eprintln!("orch: {}", failure.message());
        }
    }
}

fn converge_supervision_residual(details: &mut SupervisionDetails) {
    let Some(mut residual) = details.residual_child.take() else {
        return;
    };
    let (evidence, failure_message) = match residual.converge_residual_tree() {
        Ok(evidence) => (evidence, None),
        Err(failure) => {
            let message = failure.message();
            (failure.evidence, Some(message))
        }
    };
    extend_unique_signals(&mut details.outcome.signals, evidence.signals);
    #[cfg(test)]
    details.signal_trace.extend(evidence.signal_trace);
    for pgid in evidence.termination_order {
        if !details.termination_order.contains(&pgid) {
            details.termination_order.push(pgid);
        }
    }
    if !details.outcome.signals.is_empty() {
        details.outcome.exited_naturally = false;
    }
    details
        .outcome
        .set_managed_scope_terminated(failure_message.is_none());
    details.exit_status = evidence.exit_status.or(details.exit_status);
    details.owned_helpers = details.owned_helpers.max(evidence.owned_helpers);
    details.topology_snapshots = evidence.metrics.topology_snapshots;
    details.full_credential_inspections = evidence.metrics.full_credential_inspections;
    #[cfg(test)]
    {
        details.containment_receipts_observed = details
            .containment_receipts_observed
            .max(evidence.containment_receipts_observed);
        details.pending_containment_groups_observed = details
            .pending_containment_groups_observed
            .max(evidence.pending_containment_groups_observed);
    }
    if let Some(failure_message) = failure_message {
        let combined = details.error.take().map_or_else(
            || failure_message.clone(),
            |existing| format!("{existing}; {failure_message}"),
        );
        details.error = Some(combined);
        // Preserve exact Child custody for diagnostics/manual recovery. The
        // latched owner makes its eventual Drop read-only and signal-free.
        details.residual_child = Some(residual);
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise_managed_child_with_control(
    child: Child,
    program: &str,
    log_path: &Path,
    policy: WakeSupervisorPolicy,
    baseline_topology: Vec<ManagedProcessTopology>,
    provider_credential: Option<ManagedProcessCredential>,
    initial_error: Option<String>,
    isolated_session: bool,
    observer: ProcessObserver,
    runtime_limit: ManagedWakeRuntimeLimit,
    provider_started_at: Instant,
    mut control: Option<ManagedWakeControlRuntime>,
    initial_winner: Option<ManagedWakeStopReason>,
    mut session_receipt_sink: Option<ManagedSessionReceiptSink>,
) -> Result<SupervisionDetails> {
    let mut owner = ManagedTreeOwner::new(
        child,
        baseline_topology,
        provider_credential,
        isolated_session,
        observer,
    );
    let provider_pgid = owner.provider_pgid;
    let (mut tail, tail_error) = match ManagedLogTail::open(log_path) {
        Ok(tail) => (Some(tail), None),
        Err(tail_error) => (
            None,
            Some(format!(
                "managed wake log could not be opened: {tail_error:#}"
            )),
        ),
    };
    let mut error = initial_error.or_else(|| {
        (policy.poll_interval_ms == 0)
            .then(|| "wake supervisor poll interval must be non-zero".to_string())
    });
    let canonical_runtime_limit =
        managed_wake_runtime_limit(runtime_limit.requested_review_deadline_secs);
    if runtime_limit != canonical_runtime_limit || runtime_limit.effective_secs == 0 {
        error.get_or_insert_with(|| "wake supervisor runtime limit is not canonical".to_string());
    }
    let hard_deadline =
        provider_started_at.checked_add(Duration::from_secs(runtime_limit.effective_secs));
    if hard_deadline.is_none() {
        error.get_or_insert_with(|| "wake supervisor hard deadline overflowed".to_string());
    }
    if error.is_none() {
        error = tail_error;
    }
    // Never `try_wait` an isolated provider merely because its own PGID looks
    // singleton: an unobserved child may already be live in another PGID of
    // the same session. Preserve the direct Child zombie and recover its
    // birth-bearing epoch receipt before any reap. The legacy receipt-less
    // singleton reap remains only for non-isolated public supervision, where
    // no SID-wide ownership claim is made.
    if owner.owned.get(&provider_pgid).is_none() && owner.child_status.is_none() {
        if owner.isolated_sid.is_some() {
            if let Err(provider_error) = owner.ensure_provider_cleanup_owned() {
                error.get_or_insert_with(|| {
                    format!("provider epoch capture failed: {provider_error:#}")
                });
            }
        } else {
            match owner.reap_uncredentialed_zombie_only_provider() {
                Ok(true) => {}
                Ok(false) => {
                    if process_group_alive_exact(provider_pgid).unwrap_or(true) {
                        if let Err(provider_error) = owner.ensure_provider_cleanup_owned() {
                            error.get_or_insert_with(|| {
                                format!("provider credential capture failed: {provider_error:#}")
                            });
                        }
                    }
                }
                Err(reap_error) => {
                    error.get_or_insert_with(|| {
                        format!("initial singleton-zombie reap proof failed: {reap_error:#}")
                    });
                }
            }
        }
    }
    let mut terminal_seen = false;
    let mut terminal_at = None;
    let mut last_frame_observed_at = None;
    let mut last_frame_summary = None;
    let mut provider_exited_without_signal = owner.child_status.is_some();
    // Four finite startup observations bind early helpers at t0/250/500/750ms.
    // Completion-relative scheduling prevents a slow Darwin census from
    // causing catch-up bursts. After warm-up, long-lived idle supervision is
    // hard-backed off to one A/full/B discovery per five seconds. Terminal,
    // provider-exit, cleanup, signal and final observations are independent.
    let mut topology_cadence = ManagedTopologyCadence::new(Instant::now());
    let mut scheduled_topology_observations = 0usize;
    let mut stop_winner = initial_winner;
    let mut hard_deadline_reached =
        matches!(stop_winner, Some(ManagedWakeStopReason::HardDeadline));

    while error.is_none() && stop_winner.is_none() {
        let now = Instant::now();
        let provider_exit_visible = owner.provider_exit_visible_or_hold();
        hard_deadline_reached |= hard_deadline.is_some_and(|deadline| now >= deadline);
        stop_winner = select_managed_wake_stop_winner(
            stop_winner,
            provider_exit_visible,
            hard_deadline_reached,
            false,
            false,
        );
        if provider_exit_visible {
            provider_exited_without_signal = true;
        }
        if stop_winner.is_some() {
            break;
        }

        if topology_cadence.is_due(now) {
            if let Err(observe_error) = owner.observe() {
                owner.observer.record_observation_failure("managed tree census/credential", &observe_error);
            }
            // Failed observations consume the same bounded cadence. They
            // cannot create a retry burst, terminal winner or cleanup demand.
            scheduled_topology_observations += 1;
            topology_cadence.record_completed_observation(Instant::now());
            owner.observer.notify_scheduled_observation_complete();
        }

        let now = Instant::now();
        let provider_exit_visible = owner.provider_exit_visible_or_hold();
        hard_deadline_reached |= hard_deadline.is_some_and(|deadline| now >= deadline);
        stop_winner = select_managed_wake_stop_winner(
            stop_winner,
            provider_exit_visible,
            hard_deadline_reached,
            false,
            false,
        );
        if provider_exit_visible {
            provider_exited_without_signal = true;
        }
        if stop_winner.is_some() {
            break;
        }

        let authenticated_cancel = match control.as_mut() {
            Some(control) => match control.poll_authenticated_cancel(stop_winner) {
                Ok(cancel) => cancel,
                Err(cancel_error) => {
                    error = Some(format!(
                        "managed wake cancel authentication failed: {cancel_error:#}"
                    ));
                    break;
                }
            },
            None => false,
        };
        stop_winner =
            select_managed_wake_stop_winner(stop_winner, false, false, authenticated_cancel, false);
        if stop_winner.is_some() {
            break;
        }

        let lines = match tail
            .as_mut()
            .expect("tail checked above")
            .read_complete_lines()
        {
            Ok(lines) => lines,
            Err(log_error) => {
                error = Some(format!("managed log tail failed: {log_error:#}"));
                break;
            }
        };
        // Do not discard the already-consumed LF-complete records when Child
        // exit becomes visible in the same scheduling instant. Winner
        // selection below still gives NaturalExit precedence, while parsing
        // these bytes preserves terminal diagnostics and triggers the exact
        // terminal-edge topology observation before cleanup.
        let mut terminal_became_visible = false;
        for line in lines {
            if let Some(summary) = managed_last_frame_summary(program, &line) {
                last_frame_observed_at = Some(Instant::now());
                last_frame_summary = Some(summary);
            }
            if let Some(sink) = session_receipt_sink.as_mut() {
                if let Err(receipt_error) = sink.observe_complete_line(&line) {
                    error = Some(format!(
                        "managed provider session receipt supervision failed: {receipt_error:#}"
                    ));
                    break;
                }
            }
            if classify_managed_terminal(program, &line) == ManagedTerminal::OpenCodeStop {
                terminal_became_visible |= !terminal_seen;
                terminal_seen = true;
                terminal_at.get_or_insert_with(Instant::now);
            }
        }
        if error.is_some() {
            break;
        }
        if terminal_became_visible {
            if let Err(observe_error) = owner.observe() {
                owner.observer.record_observation_failure("terminal-edge tree observation", &observe_error);
            }
        }
        let provider_exit_visible = owner.provider_exit_visible_or_hold();
        if provider_exit_visible {
            provider_exited_without_signal = true;
            if let Err(observe_error) = owner.observe() {
                owner.observer.record_observation_failure("child-exit tree observation", &observe_error);
            }
        }
        let now = Instant::now();
        hard_deadline_reached |= hard_deadline.is_some_and(|deadline| now >= deadline);
        let terminal_grace_elapsed = terminal_at.is_some_and(|at| {
            now.saturating_duration_since(at) >= Duration::from_millis(policy.natural_exit_grace_ms)
        });
        stop_winner = select_managed_wake_stop_winner(
            stop_winner,
            provider_exit_visible,
            hard_deadline_reached,
            false,
            terminal_grace_elapsed,
        );
        if stop_winner.is_some() {
            break;
        }
        let mut sleep_for = Duration::from_millis(policy.poll_interval_ms);
        if let Some(deadline) = hard_deadline {
            sleep_for = sleep_for.min(deadline.saturating_duration_since(now));
        }
        if let Some(terminal_at) = terminal_at {
            let terminal_deadline = terminal_at
                .checked_add(Duration::from_millis(policy.natural_exit_grace_ms))
                .unwrap_or(now);
            sleep_for = sleep_for.min(terminal_deadline.saturating_duration_since(now));
        }
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }

    let winner = stop_winner.unwrap_or(ManagedWakeStopReason::OperationalError);
    // Freeze the attribution interval at stop selection. TERM/KILL convergence
    // below can take seconds and must not be misreported as provider silence.
    let winner_observed_at = Instant::now();
    // Preserve the frozen B177 terminalSeen diagnostic when a provider writes
    // its final LF-complete record in the same instant that Child exit wins.
    // This bounded drain cannot replace the already-selected NaturalExit
    // winner and grants no additional execution grace. Deadline/cancel/error
    // winners never perform this post-winner log read.
    if winner == ManagedWakeStopReason::NaturalExit && !terminal_seen {
        match tail
            .as_mut()
            .expect("tail checked above")
            .read_complete_lines()
        {
            Ok(lines) => {
                for line in lines {
                    if let Some(summary) = managed_last_frame_summary(program, &line) {
                        last_frame_observed_at = Some(Instant::now());
                        last_frame_summary = Some(summary);
                    }
                    if let Some(sink) = session_receipt_sink.as_mut() {
                        if let Err(receipt_error) = sink.observe_complete_line(&line) {
                            error.get_or_insert_with(|| {
                                format!(
                                    "final managed session receipt supervision failed: {receipt_error:#}"
                                )
                            });
                            break;
                        }
                    }
                    terminal_seen |=
                        classify_managed_terminal(program, &line) == ManagedTerminal::OpenCodeStop;
                }
            }
            Err(log_error) => {
                error.get_or_insert_with(|| {
                    format!("final natural-exit log diagnostic failed: {log_error:#}")
                });
            }
        }
    }
    let last_frame_age_secs = last_frame_observed_at.map(|observed_at| {
        winner_observed_at
            .saturating_duration_since(observed_at)
            .as_secs()
    });
    if let Some(control) = control.as_ref() {
        if let Err(stop_error) = control.publish_stop(winner) {
            error.get_or_insert_with(|| {
                format!("publish immutable managed wake stop receipt failed: {stop_error:#}")
            });
        }
    }

    // Every operational failure still enters the same bounded convergence
    // owner. No error below is allowed to return through `?` while Child is
    // live.
    if let Err(observe_error) = owner.observe() {
        error.get_or_insert_with(|| format!("final tree observation failed: {observe_error:#}"));
    }
    let mut signals = Vec::new();
    let mut ambiguity = Vec::new();
    let mut termination_order = Vec::new();
    #[cfg(test)]
    let mut signal_trace = Vec::new();
    let mut zombie_awaiting_provider_reap = BTreeSet::new();
    let natural_helpers_gone = match owner.live_isolated_helper_groups() {
        Ok(groups) => groups.is_empty(),
        Err(census_error) => {
            error.get_or_insert_with(|| {
                format!("natural-exit isolated-session census failed: {census_error:#}")
            });
            false
        }
    };
    let mut natural_groups = owner.helper_groups();
    natural_groups.insert(provider_pgid);
    let naturally_gone = owner.child_status.is_some()
        && natural_helpers_gone
        && natural_groups
            .into_iter()
            .all(|pgid| process_group_alive_exact(pgid).is_ok_and(|alive| !alive));
    let mut provider_reap_authorized = naturally_gone;

    if !naturally_gone {
        // A non-isolated direct child with no receipted helper cannot hide a
        // cross-PGID session escape. The final observation plus exact
        // provider-group proof are sufficient; avoid four full-table scans on
        // the ordinary 1s idle path. Isolated production providers retain the
        // two quiet discovery passes.
        let mut quiet_helper_passes =
            if owner.isolated_sid.is_none() && owner.helper_groups().is_empty() {
                2u8
            } else {
                0u8
            };
        for _ in 0..64 {
            if quiet_helper_passes >= 2 {
                break;
            }
            if let Err(observe_error) = owner.observe() {
                error.get_or_insert_with(|| {
                    format!("cleanup tree observation failed: {observe_error:#}")
                });
                std::thread::sleep(Duration::from_millis(policy.poll_interval_ms.max(1)));
                continue;
            }
            let mut found_signalable = false;
            for pgid in owner.helper_groups() {
                let alive = match process_group_alive_exact(pgid) {
                    Ok(alive) => alive,
                    Err(group_error) => {
                        ambiguity.push(format!("helper pgid {pgid} query failed: {group_error:#}"));
                        continue;
                    }
                };
                if !alive {
                    continue;
                }
                let termination = {
                    let owned = &owner.owned;
                    let mut control = SystemGroupControl {
                        observer: &mut owner.observer,
                        direct_child: None,
                    };
                    terminate_revalidated_group(
                        &mut control,
                        pgid,
                        owned,
                        Duration::from_millis(policy.term_grace_ms),
                    )
                };
                #[cfg(test)]
                signal_trace.extend(
                    termination
                        .signals
                        .iter()
                        .copied()
                        .map(|signal| (pgid, signal)),
                );
                if !termination.signals.is_empty() && !termination_order.contains(&pgid) {
                    termination_order.push(pgid);
                }
                extend_unique_signals(&mut signals, termination.signals);
                if termination.zombie_awaiting_provider_reap {
                    if !termination_order.contains(&pgid) {
                        termination_order.push(pgid);
                    }
                    continue;
                }
                found_signalable = true;
                if !termination.errors.is_empty() {
                    error.get_or_insert_with(|| {
                        format!("helper pgid {pgid} revalidated termination refused")
                    });
                    extend_unique_strings(&mut ambiguity, termination.errors);
                }
                if !termination.terminated {
                    extend_unique_strings(
                        &mut ambiguity,
                        vec![format!("helper pgid {pgid} remained after termination")],
                    );
                }
            }
            if found_signalable {
                quiet_helper_passes = 0;
            } else {
                quiet_helper_passes += 1;
                if quiet_helper_passes >= 2 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(policy.poll_interval_ms));
            }
        }
        match owner.partition_live_helper_groups() {
            Ok(partition) if !partition.live.is_empty() => {
                ambiguity.push(format!(
                    "helper convergence exceeded bound with live groups: {:?}",
                    partition.live
                ));
            }
            Ok(_) => {}
            Err(census_error) => {
                error.get_or_insert_with(|| {
                    format!("post-helper exact census failed: {census_error:#}")
                });
            }
        }

        if let Err(observe_error) = owner.observe() {
            error.get_or_insert_with(|| {
                format!("provider cleanup observation failed: {observe_error:#}")
            });
        }
        if !owner.owned.contains_key(&provider_pgid)
            && process_group_alive_exact(provider_pgid).unwrap_or(true)
        {
            if let Err(provider_error) = owner.ensure_provider_cleanup_owned() {
                error.get_or_insert_with(|| {
                    format!("provider cleanup credential failed: {provider_error:#}")
                });
            }
        }
        let isolated_helpers = match owner.partition_live_helper_groups() {
            Ok(partition) => {
                for pgid in &partition.owned_zombie_awaiting_provider_reap {
                    if !termination_order.contains(pgid) {
                        termination_order.push(*pgid);
                    }
                }
                zombie_awaiting_provider_reap.extend(partition.all_zombies());
                Some(partition)
            }
            Err(census_error) => {
                error.get_or_insert_with(|| {
                    format!("pre-provider isolated-session census failed: {census_error:#}")
                });
                ambiguity.push(format!(
                    "provider pgid {provider_pgid} signal refused because helper census failed"
                ));
                None
            }
        };
        if isolated_helpers
            .as_ref()
            .is_some_and(|partition| !partition.live.is_empty())
        {
            ambiguity.push(format!(
                "provider pgid {provider_pgid} signal deferred while helper groups remain: {:?}",
                isolated_helpers.as_ref().expect("checked as Some").live
            ));
        } else if isolated_helpers.is_some()
            && process_group_alive_exact(provider_pgid).unwrap_or(true)
        {
            let termination = {
                let owned = &owner.owned;
                let mut control = SystemGroupControl {
                    observer: &mut owner.observer,
                    direct_child: owner
                        .child
                        .as_mut()
                        .map(|child| (child, &mut owner.child_status)),
                };
                terminate_revalidated_group(
                    &mut control,
                    provider_pgid,
                    owned,
                    Duration::from_millis(policy.term_grace_ms),
                )
            };
            #[cfg(test)]
            signal_trace.extend(
                termination
                    .signals
                    .iter()
                    .copied()
                    .map(|signal| (provider_pgid, signal)),
            );
            if (!termination.signals.is_empty() || termination.zombie_awaiting_provider_reap)
                && !termination_order.contains(&provider_pgid)
            {
                termination_order.push(provider_pgid);
            }
            extend_unique_signals(&mut signals, termination.signals);
            if !termination.errors.is_empty() {
                error.get_or_insert_with(|| {
                    format!("provider pgid {provider_pgid} revalidated termination refused")
                });
                extend_unique_strings(&mut ambiguity, termination.errors);
            }
            provider_reap_authorized =
                termination.terminated || termination.zombie_awaiting_provider_reap;
            if !termination.terminated && !termination.zombie_awaiting_provider_reap {
                extend_unique_strings(
                    &mut ambiguity,
                    vec![format!(
                        "provider pgid {provider_pgid} remained after termination"
                    )],
                );
            }
        }
        if process_group_alive_exact(provider_pgid).is_ok_and(|alive| !alive) {
            provider_reap_authorized = true;
        }
    }

    // Cross-PGID zombies cannot be killed. Once the direct provider is gone,
    // its child table is destroyed and the kernel reparent/reaper path can
    // retire them. Keep the proof bounded here; residual custody below keeps
    // retrying fail-closed if the kernel has not converged yet.
    if provider_reap_authorized && !zombie_awaiting_provider_reap.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let _ = owner.poll_child();
            zombie_awaiting_provider_reap
                .retain(|pgid| process_group_alive_exact(*pgid).unwrap_or(true));
            if zombie_awaiting_provider_reap.is_empty() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    if provider_reap_authorized && owner.child_status.is_none() {
        match owner
            .child
            .as_mut()
            .context("managed child owner is empty")
            .and_then(|child| {
                child
                    .wait_timeout(Duration::from_secs(2))
                    .context("wait_timeout managed provider failed")
            }) {
            Ok(status) => owner.child_status = status,
            Err(wait_error) => {
                error.get_or_insert_with(|| format!("managed child wait failed: {wait_error:#}"));
            }
        }
    }
    if owner.child_status.is_none()
        && process_group_alive_exact(provider_pgid).is_ok_and(|alive| !alive)
    {
        match owner
            .child
            .as_mut()
            .context("managed child owner is empty")
            .and_then(|child| child.wait().context("reap managed provider failed"))
        {
            Ok(status) => owner.child_status = Some(status),
            Err(wait_error) => {
                error.get_or_insert_with(|| format!("managed child reap failed: {wait_error:#}"));
            }
        }
    }
    let (remaining_helper_groups, helper_census_ok) = match owner.live_isolated_helper_groups() {
        Ok(groups) => (groups, true),
        Err(census_error) => {
            ambiguity.push(format!(
                "final isolated-session helper census failed: {census_error:#}"
            ));
            (Vec::new(), false)
        }
    };
    let provider_group_gone = process_group_alive_exact(provider_pgid)
        .map(|alive| !alive)
        .unwrap_or(false);
    if !remaining_helper_groups.is_empty() {
        ambiguity.push(format!(
            "owned helper groups remain after convergence: {:?}",
            remaining_helper_groups
        ));
    }
    if !provider_group_gone {
        ambiguity.push(format!(
            "provider group {provider_pgid} remains after convergence"
        ));
    }
    let process_tree_terminated = ambiguity.is_empty()
        && owner.child_status.is_some()
        && helper_census_ok
        && provider_group_gone
        && remaining_helper_groups.is_empty();
    let exit_status = owner.child_exit_code();
    let owned_helpers = owner.ever_owned_helpers.len();
    let metrics = owner.observer.metrics;
    let observation_diagnostics = owner.observer.diagnostics.clone();
    #[cfg(test)]
    let containment_receipts_observed = owner.containment_receipts_observed;
    #[cfg(test)]
    let pending_containment_groups_observed = owner.pending_containment_groups_observed;
    if process_tree_terminated {
        owner.release_reaped_child();
    }
    let residual_child = owner.child.is_some().then_some(owner);
    let log_bytes_read = tail.as_ref().map_or(0, |tail| tail.bytes_read);
    let cancel_request = control.and_then(|control| control.cancel_request);
    let mut outcome = WakeSupervisorOutcome::canonical(
        terminal_seen,
        (naturally_gone || provider_exited_without_signal)
            && signals.is_empty()
            && process_tree_terminated,
        signals,
        process_tree_terminated,
    );
    outcome.hard_deadline_reached = hard_deadline_reached;
    outcome.completion_reason = winner;

    Ok(SupervisionDetails {
        outcome,
        owned_helpers,
        ambiguity,
        exit_status,
        termination_order,
        #[cfg(test)]
        signal_trace,
        error,
        observation_diagnostics,
        topology_snapshots: metrics.topology_snapshots,
        scheduled_topology_observations,
        #[cfg(test)]
        containment_receipts_observed,
        #[cfg(test)]
        pending_containment_groups_observed,
        full_credential_inspections: metrics.full_credential_inspections,
        log_bytes_read,
        last_frame_age_secs,
        last_frame_summary,
        cancel_request,
        residual_child,
    })
}

fn atomic_create_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_create_json_before_install(path, value, || Ok(()))
}

fn atomic_publish_bytes_before_install<F>(
    path: &Path,
    bytes: &[u8],
    before_install: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let parent = path.parent().context("supervisor sidecar has no parent")?;
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("supervisor sidecar filename is not UTF-8")?;
    let preferred = path.with_extension(format!("tmp-{}", std::process::id()));
    let open_temp = |candidate: &Path| {
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(candidate)
    };
    let (tmp, mut file) = match open_temp(&preferred) {
        Ok(file) => (preferred, file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let unique = parent.join(crate::util::unique_scratch_name(&format!("{stem}.tmp")));
            let file = open_temp(&unique).with_context(|| {
                format!(
                    "create unique supervisor sidecar temp failed: {}",
                    unique.display()
                )
            })?;
            (unique, file)
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "create supervisor sidecar temp failed: {}",
                    preferred.display()
                )
            });
        }
    };
    let prepare: Result<()> = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = prepare {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    let install = (|| {
        before_install()?;
        // hard_link is an atomic no-replace install on the same directory:
        // unlike rename, it fails if a racing writer created the target after
        // validation. The temporary name is removed only after the final link
        // exists and the directory is then fsynced.
        fs::hard_link(&tmp, path).with_context(|| {
            format!(
                "install fresh supervisor sidecar failed: {}",
                path.display()
            )
        })?;
        fs::remove_file(&tmp)
            .with_context(|| format!("remove installed sidecar temp failed: {}", tmp.display()))?;
        File::open(path.parent().context("sidecar has no parent")?)?.sync_all()?;
        Ok(())
    })();
    if install.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    install
}

fn atomic_create_json_before_install<F>(
    path: &Path,
    value: &impl Serialize,
    before_install: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    let mut bytes = serde_json::to_vec(value).context("serialize supervisor sidecar failed")?;
    bytes.push(b'\n');
    atomic_publish_bytes_before_install(path, &bytes, before_install)
}

/// Classify a registered wake command by provider topology.
///
/// Unknown providers are rejected. Agy is admitted only through its distinct
/// managed-pid-group variant, whose caller must complete the canary preflight
/// before the formal supervisor is spawned.
#[cfg(feature = "selfhost")]
pub fn durable_identity_kind(argv: &[String]) -> Result<DurableIdentityKind> {
    let program = argv.first().context(
        "durable provider topology undefined: empty argv; topology decision required before pool admission",
    )?;
    let basename = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if matches!(basename, "codex" | "opencode") {
        return Ok(DurableIdentityKind::ManagedPidGroup);
    }
    if managed_stream_harness(argv).is_some() {
        return Ok(DurableIdentityKind::ManagedPidGroup);
    }
    if is_exact_agy_provider_argv(argv) {
        return Ok(DurableIdentityKind::ManagedPidGroup);
    }
    if argv.iter().any(|arg| {
        Path::new(arg).file_name().and_then(|name| name.to_str()) == Some("wake-multica.sh")
    }) {
        return Ok(DurableIdentityKind::WakeLogProxy);
    }
    bail!(
        "durable provider topology undefined for {basename:?}; explicit topology is required before pool admission"
    )
}

fn exact_managed_wrapper_arg(value: &str, relative: &str) -> bool {
    if value == relative {
        return true;
    }
    let path = Path::new(value);
    if !path.is_absolute() || !path.ends_with(relative) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    !metadata.file_type().is_symlink()
        && metadata.is_file()
        && fs::canonicalize(path).is_ok_and(|canonical| canonical == path)
}

/// Resolve the **process topology** of a registered stream wrapper: which harness
/// owns the direct child tree the supervisor will reap.
///
/// Only the wrappers' declared registry paths in script position are accepted, so
/// an arbitrary basename or an injected message can never change topology.  This
/// is the classifier every custody/admission decision must use, independent of
/// whether that harness also emits a backend receipt.
fn managed_stream_harness(argv: &[String]) -> Option<crate::harness::HarnessId> {
    let wrapper = argv.get(1)?;
    if exact_managed_wrapper_arg(wrapper, PI_STREAM_SCRIPT_REL) {
        Some(crate::harness::HarnessId::Pi)
    } else if exact_managed_wrapper_arg(wrapper, ZCODE_STREAM_SCRIPT_REL) {
        Some(crate::harness::HarnessId::ZCode)
    } else if exact_managed_wrapper_arg(wrapper, DSH_STREAM_SCRIPT_REL) {
        Some(crate::harness::HarnessId::Dsh)
    } else {
        None
    }
}

#[cfg(feature = "selfhost")]
fn is_exact_agy_provider_argv(argv: &[String]) -> bool {
    argv.len() == 8
        && is_agy_program(argv)
        && argv[1] == "-p"
        && !argv[2].is_empty()
        && argv[3] == "--model"
        && !argv[4].is_empty()
        && argv[5] == "--effort"
        && !argv[6].is_empty()
        && argv[7] == "--dangerously-skip-permissions"
}

#[cfg(feature = "selfhost")]
fn is_agy_program(argv: &[String]) -> bool {
    argv.first().is_some_and(|program| {
        Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            == Some("agy")
    })
}

fn validate_agent_component(agent: &str) -> Result<()> {
    let safe = !agent.is_empty()
        && agent
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && agent
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !safe {
        bail!("agent 必须是单一安全 component: {agent:?}");
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(feature = "selfhost")]
fn resolve_managed_program(program: &str, argv: &[String]) -> Result<PathBuf> {
    let requested = Path::new(program);
    let resolved = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        if requested.components().count() != 1 {
            bail!("managed provider program must be absolute or a bare name");
        }
        let path = std::env::var_os("PATH").context("PATH is unavailable for managed provider")?;
        let mut found = None;
        for component in std::env::split_paths(&path) {
            if !component.is_absolute() {
                bail!("managed provider PATH contains a non-absolute component");
            }
            let candidate = component.join(requested);
            if candidate.exists() {
                found = Some(candidate);
                break;
            }
        }
        found.context("managed provider executable not found in absolute-component PATH")?
    };
    let canonical = fs::canonicalize(&resolved).with_context(|| {
        format!(
            "canonicalize managed provider failed: {}",
            resolved.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if !metadata.file_type().is_file() || metadata.mode() & 0o111 == 0 {
        bail!("managed provider must resolve to a regular executable");
    }
    let direct_provider = matches!(
        canonical.file_name().and_then(|name| name.to_str()),
        Some("codex" | "opencode" | "agy")
    );
    // D-25 (r78 planner direct fix): same topology-vs-receipt confusion as the
    // supervisor spec validator above.  The question here is whether the resolved
    // binary is the shell of a wrapper the runtime owns, which is decided by the
    // stream-harness topology and never by the backend receipt grammar.
    let managed_wrapper = (managed_stream_harness(argv).is_some()
        || matches!(
            durable_identity_kind(argv),
            Ok(DurableIdentityKind::WakeLogProxy)
        ))
        && matches!(
            canonical.file_name().and_then(|name| name.to_str()),
            Some("sh" | "bash")
        );
    if !direct_provider && !managed_wrapper {
        bail!(
            "managed provider must be codex/opencode/agy or the shell of a code-owned managed/proxy wrapper topology"
        );
    }
    Ok(canonical)
}

fn fresh_supervisor_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .context("open /dev/urandom for managed wake token failed")?
        .read_exact(&mut bytes)
        .context("read 128-bit managed wake token from /dev/urandom failed")?;
    Ok(hex::encode(bytes))
}

const DSH_DIRECT_OVERRIDE_KEYS: [&str; 3] =
    ["ORCH_DSH_PRESET", "ORCH_DSH_PROFILE", "ORCH_DSH_ZSTD_BIN"];

fn scrub_managed_dsh_overrides(command: &mut Command, argv: &[String]) {
    if managed_stream_harness(argv) == Some(crate::harness::HarnessId::Dsh) {
        for key in DSH_DIRECT_OVERRIDE_KEYS {
            command.env_remove(key);
        }
    }
}

/// Hidden CLI implementation. This path is entered before stale-binary and
/// active-round preflight; it owns exactly one provider child and writes no
/// ledger events.
fn read_bounded_launch_frame<R: BufRead>(input: &mut R) -> Result<Vec<u8>> {
    const MAX_LAUNCH_SPEC_BYTES: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_LAUNCH_SPEC_BYTES + 2) as u64)
        .read_until(b'\n', &mut bytes)
        .context("read framed wake supervisor launch spec from stdin failed")?;
    if bytes.len() > MAX_LAUNCH_SPEC_BYTES + 1 || !bytes.ends_with(b"\n") {
        bail!("wake supervisor launch spec must be one LF-framed document <= 1 MiB");
    }
    bytes.pop();
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Ok(bytes)
}

/// Consume one authenticated launch frame and supervise its exact child tree.
/// Observation errors remain diagnostic; control requests do not grant native completion.
/// The caller supplies its own selfhost feature, since a UI dependency may unify
/// the host feature without enabling task actions in a default CLI binary.
pub fn run_wake_supervisor_from_stdin(root: &Path, caller_selfhost_enabled: bool) -> Result<()> {
    let mut input = BufReader::new(io::stdin());
    let bytes = read_bounded_launch_frame(&mut input)?;
    let spec = parse_wake_supervisor_launch_spec(root, &bytes).map_err(anyhow::Error::msg)?;
    if (!caller_selfhost_enabled || !cfg!(feature = "selfhost"))
        && (spec.channel_binding.as_ref().is_none_or(|binding| !binding.standalone)
            || crate::has_selfhost_state(root)?) {
        bail!("default managed supervisor requires a standalone immutable channel binding before spawn");
    }
    if let Some(facts) = invocation_facts_from_launch(&spec) { validate_invocation_facts(root, &facts)?; }
    if spec.launch_variant != WakeSupervisorLaunchVariant::ManagedRevision3 {
        bail!("production wake supervisor rejects legacy revision 2 launch specs before spawn");
    }
    let wake_id = spec
        .wake_id
        .as_deref()
        .context("managed revision 3 launch lost wakeId")?
        .to_string();
    let runtime_limit = spec.runtime_limit;
    let supervisor_pid = std::process::id();
    let supervisor_topology = inspect_topology(supervisor_pid)?
        .context("wake supervisor disappeared before provider spawn")?;
    if supervisor_topology.pgid != supervisor_pid || supervisor_topology.sid != supervisor_pid {
        bail!("wake supervisor must establish an isolated PID=PGID=SID before provider spawn");
    }
    let baseline_topology = if spec.baseline_topology.is_empty() {
        spec.baseline
            .iter()
            .map(|credential| ManagedProcessTopology {
                pid: credential.pid,
                observed_ppid: credential.observed_ppid,
                pgid: credential.pgid,
                sid: credential.sid,
                uid: credential.uid,
                birth_identity: credential.birth_identity.clone(),
                zombie: false,
                executable_hint: credential.executable_summary.clone(),
            })
            .collect()
    } else {
        spec.baseline_topology.clone()
    };
    let program_name = spec.argv[0].clone();
    let stdout = fs::OpenOptions::new()
        .append(true)
        .open(&spec.log_path)
        .context("open managed wake log failed")?;
    let stderr = stdout
        .try_clone()
        .context("clone managed wake log failed")?;
    validate_channel_supervisor_live_binding(root, &spec)?;
    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);
    if spec.environment_is_complete {
        command.env_clear();
    }
    command.envs(&spec.env);
    // Legacy DSH launches scrub ambient/direct aliases after applying their
    // rendered map. A channel-bound launch was already validated against its
    // complete digest-bound env and must not be mutated after that check.
    if spec.channel_binding.is_none() {
        scrub_managed_dsh_overrides(&mut command, &spec.argv);
    }
    command
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_isolated_session(&mut command);
    let child = command.spawn().context("spawn managed provider failed")?;
    let provider_started_at = Instant::now();
    let mut spawned_owner = ManagedTreeOwner::new(
        child,
        baseline_topology.clone(),
        None,
        true,
        ProcessObserver::default(),
    );
    let provider_pid = spawned_owner.provider_pid;
    // Child ownership exists before the first fallible identity operation.
    // Capture failure becomes a Closing reason; it never returns through `?`.
    let provider_capture = capture_initial_provider_credential(&mut spawned_owner.observer, provider_pid);
    let (offered_credential, mut initial_error) = match provider_capture {
        Ok(credential) if credential.sid == provider_pid => (Some(credential), None),
        Ok(_) => (
            None,
            Some("provider did not establish an isolated PID=PGID=SID".to_string()),
        ),
        Err(error) => (
            None,
            Some(format!("provider pre-ACK identity failed: {error:#}")),
        ),
    };
    let mut provider_credential = offered_credential.clone();
    if provider_credential.is_none() {
        match capture_isolated_zombie_provider_epoch(provider_pid) {
            Ok(Some(epoch)) => provider_credential = Some(epoch),
            Ok(None) => {}
            Err(error) => {
                initial_error.get_or_insert_with(|| {
                    format!("provider pre-ACK zombie epoch proof failed: {error:#}")
                });
            }
        }
    }
    if let Some(credential) = provider_credential.as_ref() {
        spawned_owner
            .owned
            .insert(credential.pid, credential.clone());
    }

    let mut control_runtime = match ManagedWakeControlRuntime::publish(
        root,
        &wake_id,
        &spec.token,
        supervisor_pid,
        provider_pid,
        &spec.status_path,
        runtime_limit,
        provider_started_at,
        match (
            spec.attach_action_id.as_deref(),
            spec.continued_from_wake_id.as_deref(),
        ) {
            (Some(action_id), Some(source_wake_id)) => Some(ManagedWakeAttachDescriptorBinding {
                action_id,
                source_wake_id,
            }),
            _ => None,
        },
            invocation_facts_from_launch(&spec),
    ) {
        Ok(control) => Some(control),
        Err(error) => {
            initial_error.get_or_insert_with(|| {
                format!("publish managed wake control descriptor failed: {error:#}")
            });
            None
        }
    };
    let hard_deadline =
        provider_started_at.checked_add(Duration::from_secs(runtime_limit.effective_secs));
    if hard_deadline.is_none() {
        initial_error.get_or_insert_with(|| {
            "managed wake hard deadline overflowed after provider spawn".to_string()
        });
    }
    let mut handshake_winner = None;

    if let Some(offered_credential) = offered_credential.filter(|_| initial_error.is_none()) {
        let ack = WakeSupervisorAck {
            token: spec.token.clone(),
            phase: "OFFER".to_string(),
            protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
            supervisor_pid: std::process::id(),
            provider_pid,
            pgid: provider_pid,
            provider_credential: offered_credential.clone(),
        };
        match atomic_create_json(&spec.ack_path, &ack) {
            Ok(()) => {
                let (send, receive) = std::sync::mpsc::sync_channel(1);
                let control_thread = std::thread::Builder::new()
                    .name(format!("orch-supervisor-accept-{provider_pid}"))
                    .spawn(move || {
                        let mut control = String::new();
                        let result = input
                            .read_line(&mut control)
                            .map(|_| control)
                            .context("read supervisor ACCEPT control failed");
                        let _ = send.send(result);
                    });
                match control_thread {
                    Ok(_) => {
                        let accept_deadline = Instant::now() + Duration::from_secs(6);
                        let mut received = None;
                        loop {
                            let provider_exited =
                                match spawned_owner.provider_exit_visible_without_reap() {
                                    Ok(exited) => exited,
                                    Err(error) => {
                                        initial_error = Some(format!(
                                            "provider exit proof failed during ACCEPT: {error:#}"
                                        ));
                                        break;
                                    }
                                };
                            let deadline_reached =
                                hard_deadline.is_some_and(|deadline| Instant::now() >= deadline);
                            let authenticated_cancel = match control_runtime.as_mut() {
                                Some(control) => {
                                    match control.poll_authenticated_cancel(handshake_winner) {
                                        Ok(cancel) => cancel,
                                        Err(error) => {
                                            initial_error = Some(format!(
                                            "cancel authentication failed during ACCEPT: {error:#}"
                                        ));
                                            break;
                                        }
                                    }
                                }
                                None => false,
                            };
                            handshake_winner = select_managed_wake_stop_winner(
                                handshake_winner,
                                provider_exited,
                                deadline_reached,
                                authenticated_cancel,
                                false,
                            );
                            if handshake_winner.is_some() {
                                break;
                            }
                            match receive.try_recv() {
                                Ok(result) => {
                                    received = Some(result);
                                    break;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    initial_error =
                                        Some("parent ACCEPT reader disconnected".to_string());
                                    break;
                                }
                            }
                            let now = Instant::now();
                            if now >= accept_deadline {
                                initial_error = Some(
                                    "parent ACCEPT timed out; provider ownership rejected"
                                        .to_string(),
                                );
                                break;
                            }
                            let mut sleep_for = Duration::from_millis(5)
                                .min(accept_deadline.saturating_duration_since(now));
                            if let Some(deadline) = hard_deadline {
                                sleep_for = sleep_for.min(deadline.saturating_duration_since(now));
                            }
                            if !sleep_for.is_zero() {
                                std::thread::sleep(sleep_for);
                            }
                        }
                        match received {
                            Some(Ok(control))
                                if control == format!("ACCEPT {}\n", spec.token)
                                    && !spec.ack_path.exists() =>
                            {
                                match sample_provider_credential(
                                    &mut spawned_owner.observer,
                                    provider_pid,
                                ) {
                                    Ok(fresh) if fresh == offered_credential => {
                                        spawned_owner.owned.insert(provider_pid, fresh.clone());
                                        provider_credential = Some(fresh.clone());
                                        let accepted = WakeSupervisorAck {
                                            token: spec.token.clone(),
                                            phase: "ACCEPTED".to_string(),
                                            protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
                                            supervisor_pid: std::process::id(),
                                            provider_pid,
                                            pgid: provider_pid,
                                            provider_credential: fresh,
                                        };
                                        if let Err(error) =
                                            atomic_create_json(&spec.ack_path, &accepted)
                                        {
                                            initial_error = Some(format!(
                                            "publish wake supervisor ACCEPTED failed: {error:#}"
                                        ));
                                        }
                                    }
                                    Ok(fresh)
                                        if launch_guard_cleanup_refresh_matches(
                                            &offered_credential,
                                            &fresh,
                                        ) =>
                                    {
                                        // The complete OFFER no longer matches,
                                        // so ownership transfer remains rejected.
                                        // The immutable direct-child epoch is
                                        // unchanged, however, and the fresh full
                                        // receipt is valid solely for bounded
                                        // cleanup of that rejected launch.
                                        spawned_owner.owned.insert(provider_pid, fresh.clone());
                                        provider_credential = Some(fresh);
                                        initial_error = Some(
                                            "provider credential changed between OFFER and ACCEPT"
                                                .to_string(),
                                        );
                                    }
                                    Ok(_) => {
                                        initial_error = Some(
                                            "provider credential changed between OFFER and ACCEPT"
                                                .to_string(),
                                        );
                                    }
                                    Err(error) => {
                                        initial_error = Some(format!(
                                            "provider ACCEPT identity proof failed: {error:#}"
                                        ));
                                    }
                                }
                            }
                            Some(Ok(_)) => {
                                initial_error = Some(
                                "parent ACCEPT token/order mismatch; provider ownership rejected"
                                    .to_string(),
                            );
                            }
                            Some(Err(error)) => {
                                initial_error =
                                    Some(format!("parent ACCEPT read failed: {error:#}"));
                            }
                            None => {}
                        }
                    }
                    Err(error) => {
                        initial_error = Some(format!("spawn ACCEPT reader failed: {error}"));
                    }
                }
            }
            Err(error) => {
                initial_error = Some(format!("publish wake supervisor OFFER failed: {error:#}"));
            }
        }
    }
    if (initial_error.is_some() || handshake_winner.is_some())
        && spec.ack_path.exists()
        && path_is_regular_nonsymlink(&spec.ack_path).unwrap_or(false)
    {
        let _ = fs::remove_file(&spec.ack_path);
    }

    let provider_metrics = spawned_owner.observer.metrics;
    let session_receipt_sink = if Path::new(&program_name)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("opencode")
    {
        match (control_runtime.as_ref(), spec.agent.as_deref()) {
            (Some(control), Some(agent)) => Some(ManagedSessionReceiptSink::new(
                &control.dir,
                &wake_id,
                &spec.token,
                agent,
                &program_name,
            )?),
            _ => None,
        }
    } else {
        None
    };
    let child = spawned_owner
        .take_child()
        .context("spawned provider owner lost Child before supervision")?;
    let mut details = supervise_managed_child_with_control(
        child,
        &program_name,
        &spec.log_path,
        spec.policy,
        baseline_topology,
        provider_credential,
        initial_error,
        true,
        ProcessObserver::default(),
        runtime_limit,
        provider_started_at,
        control_runtime,
        handshake_winner,
        session_receipt_sink,
    )?;
    // Residual custody may turn an initially ambiguous/faulted convergence
    // into a fully reaped tree. Merge its actual signals/order/exit/metrics
    // before writing the immutable status; preserve the original error and
    // ambiguity as diagnostic evidence.
    converge_supervision_residual(&mut details);
    let containment = details.outcome.normalized_containment();
    debug_assert_eq!(details.outcome.containment_claim, containment.claim);
    let status = WakeSupervisorStatus {
        protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
        token: spec.token.clone(),
        wake_id: wake_id.clone(),
        agent: spec.agent.clone(),
        runtime_limit,
        completion_reason: details.outcome.completion_reason,
        hard_deadline_reached: details.outcome.hard_deadline_reached,
        cancel_request_id: details
            .cancel_request
            .as_ref()
            .map(|request| request.request_id.clone()),
        cancel_reason: details
            .cancel_request
            .as_ref()
            .map(|request| request.reason.clone()),
        terminal_seen: details.outcome.terminal_seen,
        exited_naturally: details.outcome.exited_naturally,
        signals: details
            .outcome
            .signals
            .iter()
            .map(|signal| match signal {
                super::process::TerminationSignal::Term => "TERM".to_string(),
                super::process::TerminationSignal::Kill => "KILL".to_string(),
            })
            .collect(),
        containment_capability: containment.capability,
        containment_claim: containment.claim,
        managed_scope_terminated: containment.managed_scope_terminated,
        fork_complete: containment.fork_complete,
        process_tree_terminated: containment.legacy_process_tree_terminated,
        owned_helpers: details.owned_helpers,
        termination_order: details.termination_order,
        ambiguity: details.ambiguity,
        exit_status: details.exit_status,
        error: details.error.clone(),
        observation_diagnostics: details.observation_diagnostics.clone(),
        topology_snapshots: details.topology_snapshots + provider_metrics.topology_snapshots,
        full_credential_inspections: details.full_credential_inspections
            + provider_metrics.full_credential_inspections,
        log_bytes_read: details.log_bytes_read,
        last_frame_age_secs: details.last_frame_age_secs,
        last_frame_summary: details.last_frame_summary,
        elapsed_secs: Some(provider_started_at.elapsed().as_secs()),
    };
    let status_result = atomic_create_json(&spec.status_path, &status);
    status_result?;
    if let Some(error) = details.error {
        bail!("wake supervisor closed after error: {error}");
    }
    if !containment.managed_scope_terminated {
        bail!("wake supervisor could not prove managed-scope termination");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WakeOwnerReapRecord {
    state: String,
    owner_kind: String,
    pid: u32,
    pgid: u32,
    registered_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reaped_at: Option<String>,
}

impl WakeOwnerKind {
    fn sidecar_name(self) -> &'static str {
        match self {
            Self::ManagedSupervisor => "managedSupervisor",
            #[cfg(feature = "selfhost")]
            Self::WakeLogProxy => "wakeLogProxy",
        }
    }
}

fn atomic_replace_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("reap sidecar has no parent")?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat waiting reap sidecar failed: {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o777 != 0o600
    {
        bail!("waiting reap sidecar type/owner/mode is invalid");
    }
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("reap sidecar filename is not UTF-8")?;
    let tmp = parent.join(crate::util::unique_scratch_name(&format!("{stem}.replace")));
    let replace = (|| {
        let mut bytes = serde_json::to_vec(value).context("serialize reaped sidecar failed")?;
        bytes.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create reaped sidecar temp failed: {}", tmp.display()))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
            .with_context(|| format!("replace reaped sidecar failed: {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if replace.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    replace
}

struct PreparedWakeOwnerReaper {
    sender: std::sync::mpsc::SyncSender<Child>,
    registered: std::sync::mpsc::Receiver<std::result::Result<(), String>>,
}

fn prepare_wake_owner_reaper(
    root: &Path,
    token: &str,
    owner_kind: WakeOwnerKind,
) -> Result<PreparedWakeOwnerReaper> {
    if !is_lower_hex_token(token) && !is_strict_uuid(token) {
        bail!("wake owner reap token must be a strict supervisor token or wakeId");
    }
    let supervisor_dir = authenticated_supervisor_dir(root, true)?;
    let sidecar_path = supervisor_dir.join(format!("{token}.reap.json"));
    if sidecar_path.exists() {
        bail!("wake owner reap sidecar token collision");
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Child>(1);
    let (registered_tx, registered) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(format!("orch-wake-{}-reaper", owner_kind.sidecar_name()))
        .spawn(move || {
            let Ok(mut child) = receiver.recv() else {
                return;
            };
            let pid = child.id();
            let registered_at = crate::util::now_rfc3339();
            let waiting = WakeOwnerReapRecord {
                state: "waiting".to_string(),
                owner_kind: owner_kind.sidecar_name().to_string(),
                pid,
                pgid: pid,
                registered_at: registered_at.clone(),
                exit_code: None,
                exit_signal: None,
                wait_error: None,
                reaped_at: None,
            };
            let waiting_result = atomic_create_json(&sidecar_path, &waiting)
                .map_err(|error| format!("publish waiting reap sidecar failed: {error:#}"));
            let waiting_published = waiting_result.is_ok();
            let _ = registered_tx.send(waiting_result);

            let waited = child.wait();
            if waiting_published {
                let (exit_code, exit_signal, wait_error) = match waited {
                    Ok(status) => (status.code(), status.signal(), None),
                    Err(error) => (None, None, Some(error.to_string())),
                };
                let reaped = WakeOwnerReapRecord {
                    state: "reaped".to_string(),
                    owner_kind: owner_kind.sidecar_name().to_string(),
                    pid,
                    pgid: pid,
                    registered_at,
                    exit_code,
                    exit_signal,
                    wait_error,
                    reaped_at: Some(crate::util::now_rfc3339()),
                };
                if let Err(error) = atomic_replace_json(&sidecar_path, &reaped) {
                    eprintln!(
                        "orch: exact Child pid {pid} was waited but reaped sidecar publication failed: {error:#}"
                    );
                }
            } else if let Err(error) = waited {
                eprintln!(
                    "orch: exact Child pid {pid} wait failed after waiting sidecar publication failed: {error}"
                );
            }
        })
        .context("spawn wake owner reaper thread failed")?;
    Ok(PreparedWakeOwnerReaper { sender, registered })
}

fn wait_exact_child_after_failed_handoff(mut child: Child, reason: &str) -> anyhow::Error {
    let pid = child.id();
    match child.wait() {
        Ok(status) => anyhow::anyhow!(
            "{reason}; exact Child pid {pid} retained and waited with status {status}"
        ),
        Err(error) => {
            anyhow::anyhow!("{reason}; exact Child pid {pid} retained but wait failed: {error}")
        }
    }
}

impl PreparedWakeOwnerReaper {
    fn handoff(self, child: Child) -> Result<()> {
        if let Err(error) = self.sender.send(child) {
            return Err(wait_exact_child_after_failed_handoff(
                error.0,
                "prepared wake owner reaper channel closed",
            ));
        }
        self.registered
            .recv()
            .context("wake owner reaper exited before registering exact Child")?
            .map_err(anyhow::Error::msg)
    }
}

struct SupervisorLaunchGuard {
    child: Option<Child>,
    control: Option<ChildStdin>,
    reaper: Option<PreparedWakeOwnerReaper>,
}

impl SupervisorLaunchGuard {
    fn child_mut(&mut self) -> Result<&mut Child> {
        self.child
            .as_mut()
            .context("supervisor launch guard has no Child")
    }

    fn send_accept(&mut self, token: &str) -> Result<()> {
        let mut control = self
            .control
            .take()
            .context("supervisor ACCEPT pipe is unavailable")?;
        control
            .write_all(format!("ACCEPT {token}\n").as_bytes())
            .context("write supervisor ACCEPT failed")?;
        control.flush().context("flush supervisor ACCEPT failed")?;
        drop(control);
        Ok(())
    }

    fn handoff_accepted(&mut self) -> Result<()> {
        // Take the reaper first. If it is missing, the Child remains in this
        // guard and Drop routes it through the fail-closed retain path instead
        // of briefly materializing an unowned hidden supervisor handle.
        let reaper = self
            .reaper
            .take()
            .context("supervisor launch guard lost preallocated reaper")?;
        let child = self
            .child
            .take()
            .context("supervisor launch guard lost Child before reaper handoff")?;
        reaper.handoff(child)
    }

    fn reject_and_reap(&mut self) {
        self.control.take();
        let Some(child) = self.child.take() else {
            return;
        };
        if let Some(reaper) = self.reaper.take() {
            if let Err(error) = reaper.handoff(child) {
                eprintln!("orch: rejected supervisor exact-Child cleanup failed: {error:#}");
            }
        } else {
            let error = wait_exact_child_after_failed_handoff(
                child,
                "supervisor launch guard lost its preallocated reaper",
            );
            eprintln!("orch: {error:#}");
        }
    }
}

impl Drop for SupervisorLaunchGuard {
    fn drop(&mut self) {
        self.reject_and_reap();
    }
}

// Keep rejection diagnostics available to the real CLI, not only unit-test builds.
// Fields are process identities and hashes; never include launch specs or auth tokens.
fn report_credential_drift(
    phase: &str,
    old: &ManagedProcessCredential,
    new: &ManagedProcessCredential,
    supervisor: &mut Child,
) {
    let supervisor_pid = supervisor.id();
    let supervisor_state = match supervisor.try_wait() {
        Ok(None) => "alive".to_string(),
        Ok(Some(status)) => format!("exited({status})"),
        Err(error) => format!("try-wait-error({error})"),
    };
    eprintln!(
        "orch: CREDENTIAL DRIFT phase={phase} supervisorPid={supervisor_pid} supervisorState={supervisor_state}"
    );
    macro_rules! field {
        ($name:literal, $old:expr, $new:expr) => {{
            let old = &$old;
            let new = &$new;
            eprintln!(
                "orch: CREDENTIAL FIELD {} old={:?} -> new={:?} changed={}",
                $name,
                old,
                new,
                old != new
            );
        }};
    }
    field!("pid", old.pid, new.pid);
    field!("observed_ppid", old.observed_ppid, new.observed_ppid);
    field!("pgid", old.pgid, new.pgid);
    field!("sid", old.sid, new.sid);
    field!("uid", old.uid, new.uid);
    field!("birth_identity", old.birth_identity, new.birth_identity);
    field!(
        "executable_summary",
        old.executable_summary,
        new.executable_summary
    );
}

#[cfg(test)]
fn fresh_orch_debug_bin_for_wake_tests() -> Result<PathBuf> {
    let harness = std::env::current_exe().context("resolve orch-host test harness failed")?;
    let orch = harness
        .parent()
        .and_then(Path::parent)
        .context("orch-host test harness is not under target/debug/deps")?
        .join("orch");
    let binary_modified = fs::metadata(&orch)
        .with_context(|| format!("stat test orch binary failed: {}", orch.display()))?
        .modified()
        .with_context(|| format!("read test orch binary mtime failed: {}", orch.display()))?;
    let host_manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sources = [
        host_manifest.join("src/wake.rs"),
        host_manifest.join("src/plan.rs"),
        host_manifest.join("src/legacy.rs"),
        host_manifest.join("src/generic_review.rs"),
        host_manifest.join("src/channel.rs"),
        host_manifest.join("src/channel/managed.rs"),
        host_manifest.join("src/channel/process.rs"),
        host_manifest.join("src/channel/completion.rs"),
        host_manifest.join("src/harness.rs"),
        host_manifest.join("Cargo.toml"),
        host_manifest
            .parent()
            .context("orch-host manifest has no crates parent")?
            .join("orch-cli/src/main.rs"),
    ];
    for source in sources {
        let source_modified = fs::metadata(&source)
            .with_context(|| format!("stat wake test source failed: {}", source.display()))?
            .modified()
            .with_context(|| format!("read wake test source mtime failed: {}", source.display()))?;
        if binary_modified < source_modified {
            bail!(
                "stale target/debug/orch rejected for wake test: {} is older than {}",
                orch.display(),
                source.display()
            );
        }
    }
    Ok(orch)
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum B177ForgedAckPhase {
    Offer,
    Accepted,
}

#[cfg(test)]
#[derive(Debug, Clone)]
struct B177ForgedAckCapture {
    token: String,
    provider: ManagedProcessCredential,
    supervisor: ManagedProcessCredential,
}

#[cfg(test)]
fn b177_maybe_forge_parent_ack_bytes(phase: B177ForgedAckPhase, bytes: Vec<u8>) -> Result<Vec<u8>> {
    if B177_FORGED_ACK_PHASE.with(|slot| slot.get()) != Some(phase) {
        return Ok(bytes);
    }
    let ack: WakeSupervisorAck =
        serde_json::from_slice(&bytes).context("parse original test ACK")?;
    let supervisor = inspect_stable_process(ack.supervisor_pid)?
        .context("forged-ACK hook lacks a stable supervisor receipt")?;
    let capture = B177ForgedAckCapture {
        token: ack.token.clone(),
        provider: ack.provider_credential.clone(),
        supervisor,
    };
    if B177_FORGED_ACK_CAPTURE
        .with(|slot| slot.replace(Some(capture)))
        .is_some()
    {
        bail!("forged-ACK hook fired more than once");
    }
    let mut value = serde_json::to_value(&ack)?;
    value["protocolRevision"] = serde_json::json!(WAKE_SUPERVISOR_PROTOCOL_REVISION - 1);
    serde_json::to_vec(&value).context("serialize forged test ACK")
}

#[allow(clippy::too_many_arguments)]
fn spawn_channel_wake_supervisor(
    root: &Path,
    alias: &str,
    argv: &[String],
    log_path: &Path,
    wake_id: &str,
    runtime_limit: ManagedWakeRuntimeLimit,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    binding: ChannelSupervisorBindingV1,
) -> Result<u32> {
    spawn_managed_wake_supervisor_inner(
        root,
        alias,
        argv,
        log_path,
        wake_id,
        runtime_limit,
        env,
        None,
        cwd,
        Some(binding),
    )
}

#[allow(clippy::too_many_arguments)]
fn spawn_managed_wake_supervisor_inner(
    root: &Path,
    agent: &str,
    argv: &[String],
    log_path: &Path,
    wake_id: &str,
    runtime_limit: ManagedWakeRuntimeLimit,
    env: &BTreeMap<String, String>,
    attach_binding: Option<ManagedWakeAttachDescriptorBinding<'_>>,
    cwd: &Path,
    channel_binding: Option<ChannelSupervisorBindingV1>,
) -> Result<u32> {
    let envelope_map = env
        .iter()
        .filter(|(key, _)| crate::harness::ENVELOPE_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    if !envelope_map.is_empty() {
        // Complete-or-refuse validation is deliberately before Command::spawn:
        // once the supervisor exists it may immediately start the provider.
        crate::harness::InvocationEnvelope::from_map(envelope_map)?;
    }
    let lifecycle = wake_owner_lifecycle_plan(WakeOwnerKind::ManagedSupervisor);
    if lifecycle.handoff != WakeOwnerHandoff::AfterSupervisorAck {
        bail!("managed wake supervisor lifecycle requires post-ack handoff");
    }
    let (program, rest) = argv.split_first().context("wake.argv 不能为空")?;
    let program = if let Some(binding) = channel_binding.as_ref() {
        let raw = Path::new(program);
        let metadata = fs::symlink_metadata(raw)
            .with_context(|| format!("stat channel program failed: {}", raw.display()))?;
        if !raw.is_absolute()
            || metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || crate::channel::executable_identity_digest_v1(raw)?
                != binding.program_identity_digest
        {
            bail!("channel program identity rejected before supervisor spawn");
        }
        fs::canonicalize(raw)?
    } else {
        #[cfg(feature = "selfhost")]
        { resolve_managed_program(program, argv)? }
        #[cfg(not(feature = "selfhost"))]
        { bail!("default managed transport requires an immutable channel binding"); }
    };
    if !is_strict_uuid(wake_id) {
        bail!("managed wake supervisor requires a strict parent-generated wakeId");
    }
    if runtime_limit != managed_wake_runtime_limit(runtime_limit.requested_review_deadline_secs) {
        bail!("managed wake supervisor requires a canonical finite runtime limit");
    }
    let supervisor_dir = authenticated_supervisor_dir(root, true)?;
    let token = fresh_supervisor_token()?;
    let ack_path = supervisor_dir.join(format!("{token}.ack.json"));
    let status_path = supervisor_dir.join(format!("{token}.status.json"));
    if ack_path.exists() || status_path.exists() {
        bail!("wake supervisor token collision");
    }
    let mut absolute_argv = Vec::with_capacity(argv.len());
    absolute_argv.push(program.to_string_lossy().into_owned());
    absolute_argv.extend(rest.iter().cloned());
    let baseline_topology = process_topology_snapshot()?
        .into_values()
        .collect::<Vec<_>>();
    let spec = WakeSupervisorLaunchSpec {
        version: 1,
        protocol_revision: WAKE_SUPERVISOR_PROTOCOL_REVISION,
        token: token.clone(),
        argv: absolute_argv,
        env: env.clone(),
        channel_binding,
        environment_is_complete: true,
        cwd: fs::canonicalize(cwd)?,
        log_path: log_path.to_path_buf(),
        ack_path: ack_path.clone(),
        status_path,
        policy: WakeSupervisorPolicy::production(),
        wake_id: Some(wake_id.to_string()),
        agent: Some(agent.to_string()),
        attach_action_id: attach_binding.map(|binding| binding.action_id.to_string()),
        continued_from_wake_id: attach_binding.map(|binding| binding.source_wake_id.to_string()),
        runtime_limit,
        launch_variant: WakeSupervisorLaunchVariant::ManagedRevision3,
        baseline: Vec::new(),
        baseline_topology,
    };
    // Run our typed parser before transferring the one-shot document.
    parse_wake_supervisor_launch_spec(root, &serde_json::to_vec(&spec)?)
        .map_err(anyhow::Error::msg)?;
    #[cfg(not(test))]
    let current_exe = std::env::current_exe().context("resolve current orch executable failed")?;
    #[cfg(test)]
    let current_exe = fresh_orch_debug_bin_for_wake_tests()?;
    // Reserve the one-shot reaper before the hidden supervisor (and therefore
    // before any provider) exists. Resource exhaustion now fails with zero
    // spawned process rather than forcing a choice between an orphan and an
    // unsafe leader-only kill.
    let reaper = if lifecycle.prearm_reaper {
        Some(prepare_wake_owner_reaper(
            root,
            &token,
            WakeOwnerKind::ManagedSupervisor,
        )?)
    } else {
        None
    };
    let mut supervisor_command = Command::new(current_exe);
    supervisor_command
        .args([
            "--root",
            root.to_string_lossy().as_ref(),
            "__wake-supervise",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The custodian must outlive the foreground wake/dispatch process group.
    // Establish its own PID=PGID=SID atomically before exec and before it can
    // spawn the provider. The hidden entry revalidates this invariant.
    if lifecycle.isolate_process_group {
        configure_isolated_session(&mut supervisor_command);
    }
    let mut supervisor = supervisor_command
        .spawn()
        .context("spawn detached wake supervisor failed")?;
    let control = supervisor.stdin.take();
    let mut guard = SupervisorLaunchGuard {
        child: Some(supervisor),
        control,
        reaper,
    };
    if guard.control.is_none() {
        bail!("wake supervisor stdin was not piped");
    }
    let mut encoded = serde_json::to_vec(&spec)?;
    encoded.push(b'\n');
    guard
        .control
        .as_mut()
        .context("wake supervisor control pipe missing")?
        .write_all(&encoded)
        .context("write framed wake supervisor launch spec failed")?;
    guard
        .control
        .as_mut()
        .context("wake supervisor control pipe missing")?
        .flush()
        .context("flush framed wake supervisor launch spec failed")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ack_path.exists() {
            if !path_is_regular_nonsymlink(&ack_path)? {
                bail!("wake supervisor ack is not a regular non-symlink");
            }
            let bytes =
                secure_read_lf_frame(&ack_path, &supervisor_dir, MAX_MANAGED_WAKE_CONTROL_BYTES)?;
            #[cfg(test)]
            let bytes = b177_maybe_forge_parent_ack_bytes(B177ForgedAckPhase::Offer, bytes)?;
            let ack: WakeSupervisorAck =
                serde_json::from_slice(&bytes).context("parse typed wake supervisor ack failed")?;
            if ack.token != token
                || ack.phase != "OFFER"
                || ack.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
                || ack.supervisor_pid != guard.child_mut()?.id()
                || ack.provider_pid == 0
                || ack.provider_pid != ack.pgid
                || ack.provider_credential.pid != ack.provider_pid
                || ack.provider_credential.pgid != ack.pgid
                || ack.provider_credential.sid != ack.provider_pid
            {
                bail!("wake supervisor ack identity mismatch");
            }
            let current = sample_provider_credential(&mut ProcessObserver::default(), ack.provider_pid)
                .context("parent provider credential revalidation failed")?;
            if current != ack.provider_credential {
                report_credential_drift(
                    "offer-to-parent-before-accept",
                    &ack.provider_credential,
                    &current,
                    guard.child_mut()?,
                );
                bail!("wake supervisor provider credential changed before ACCEPT");
            }
            // Deleting the exact OFFER is the durable parent-side guard. Only
            // after it succeeds may ACCEPT transfer provider ownership.
            fs::remove_file(&ack_path).context("remove consumed wake supervisor ack failed")?;
            guard.send_accept(&token)?;
            let accepted_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if ack_path.exists() {
                    if !path_is_regular_nonsymlink(&ack_path)? {
                        bail!("wake supervisor ACCEPTED ack is not a regular non-symlink");
                    }
                    let bytes = secure_read_lf_frame(
                        &ack_path,
                        &supervisor_dir,
                        MAX_MANAGED_WAKE_CONTROL_BYTES,
                    )?;
                    #[cfg(test)]
                    let bytes =
                        b177_maybe_forge_parent_ack_bytes(B177ForgedAckPhase::Accepted, bytes)?;
                    let accepted: WakeSupervisorAck = serde_json::from_slice(&bytes)
                        .context("parse typed wake supervisor ACCEPTED ack failed")?;
                    if accepted.token != token
                        || accepted.phase != "ACCEPTED"
                        || accepted.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
                        || accepted.protocol_revision != ack.protocol_revision
                        || accepted.supervisor_pid != guard.child_mut()?.id()
                        || accepted.provider_pid != ack.provider_pid
                        || accepted.pgid != ack.pgid
                        || accepted.provider_credential != ack.provider_credential
                    {
                        bail!("wake supervisor ACCEPTED identity mismatch");
                    }
                    let parent_fresh = sample_provider_credential(
                        &mut ProcessObserver::default(),
                        accepted.provider_pid,
                    )
                    .context("parent ACCEPTED provider proof failed")?;
                    if parent_fresh != accepted.provider_credential {
                        report_credential_drift(
                            "accepted-to-parent-after-accepted",
                            &accepted.provider_credential,
                            &parent_fresh,
                            guard.child_mut()?,
                        );
                        bail!("wake supervisor provider credential changed after ACCEPTED");
                    }
                    fs::remove_file(&ack_path)
                        .context("remove consumed wake supervisor ACCEPTED ack failed")?;
                    guard.handoff_accepted()?;
                    return Ok(ack.provider_pid);
                }
                if let Some(status) = guard.child_mut()?.try_wait()? {
                    bail!("wake supervisor exited before ACCEPTED (status={status})");
                }
                if Instant::now() >= accepted_deadline {
                    bail!("wake supervisor ACCEPTED timed out after 2s");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(status) = guard.child_mut()?.try_wait()? {
            bail!("wake supervisor exited before ack (status={status})");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("wake supervisor ack timed out after 5s")
}

fn git_rev_parse_path(repo: &Path, args: &[&str], label: &str) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .args(args)
        .output()
        .with_context(|| format!("{label}: git rev-parse launch failed"))?;
    if !output.status.success() {
        bail!(
            "{label}: git rev-parse failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let raw = std::str::from_utf8(&output.stdout)
        .with_context(|| format!("{label}: git rev-parse output is not UTF-8"))?
        .trim();
    if raw.is_empty() || raw.lines().count() != 1 {
        bail!("{label}: git rev-parse returned an empty/multiline path");
    }
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    fs::canonicalize(&absolute)
        .with_context(|| format!("{label}: cannot canonicalize {}", absolute.display()))
}

/// Prove that a provisioned review site is the exact linked worktree owned by
/// this repository.  A directory containing an unrelated repository (or a
/// forged `.git` symlink/directory) must never be accepted merely because its
/// HEAD happens to equal the requested SHA.
pub(crate) fn validate_review_site_repository(root: &Path, worktree: &Path) -> Result<()> {
    let gitfile = worktree.join(".git");
    let gitfile_meta = fs::symlink_metadata(&gitfile)
        .with_context(|| format!("review site lacks .git file: {}", gitfile.display()))?;
    if gitfile_meta.file_type().is_symlink() || !gitfile_meta.file_type().is_file() {
        bail!(
            "review site .git must be a regular non-symlink file: {}",
            gitfile.display()
        );
    }

    let canonical_worktree = fs::canonicalize(worktree).with_context(|| {
        format!(
            "canonicalize review worktree failed: {}",
            worktree.display()
        )
    })?;
    let reported_toplevel =
        git_rev_parse_path(worktree, &["--show-toplevel"], "review site toplevel")?;
    if reported_toplevel != canonical_worktree {
        bail!(
            "review site toplevel mismatch: expected={} actual={}",
            canonical_worktree.display(),
            reported_toplevel.display()
        );
    }

    let root_common = git_rev_parse_path(root, &["--git-common-dir"], "root common-dir")?;
    let site_common =
        git_rev_parse_path(worktree, &["--git-common-dir"], "review site common-dir")?;
    if site_common != root_common {
        bail!(
            "review site belongs to a foreign git common-dir: expected={} actual={}",
            root_common.display(),
            site_common.display()
        );
    }
    let site_git_dir = git_rev_parse_path(worktree, &["--git-dir"], "review site git-dir")?;
    if site_git_dir == site_common || !site_git_dir.starts_with(site_common.join("worktrees")) {
        bail!(
            "review site git-dir is not a linked-worktree admin dir: gitDir={} commonDir={}",
            site_git_dir.display(),
            site_common.display()
        );
    }

    let gitfile_text = fs::read_to_string(&gitfile)
        .with_context(|| format!("read review site .git file failed: {}", gitfile.display()))?;
    let gitfile_line = gitfile_text
        .strip_suffix('\n')
        .unwrap_or(gitfile_text.as_str());
    if gitfile_line.is_empty()
        || gitfile_line
            .chars()
            .any(|character| matches!(character, '\r' | '\n'))
    {
        bail!("review site .git file is not one canonical gitdir line");
    }
    let gitfile_target = gitfile_line
        .strip_prefix("gitdir: ")
        .filter(|value| !value.is_empty())
        .context("review site .git file is not one canonical gitdir line")?;
    let gitfile_target = PathBuf::from(gitfile_target);
    let gitfile_target = if gitfile_target.is_absolute() {
        gitfile_target
    } else {
        worktree.join(gitfile_target)
    };
    let canonical_gitfile_target = fs::canonicalize(&gitfile_target).with_context(|| {
        format!(
            "canonicalize review site .git target failed: {}",
            gitfile_target.display()
        )
    })?;
    if canonical_gitfile_target != site_git_dir {
        bail!(
            "review site .git target mismatch: expected={} actual={}",
            site_git_dir.display(),
            canonical_gitfile_target.display()
        );
    }
    Ok(())
}

fn is_full_review_head(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Result of explicit launch admission, not a claim that the model finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeRunOutcome {
    /// A new invocation crossed its authenticated supervisor acknowledgement.
    Spawned {
        /// Immutable identity for this managed invocation.
        wake_id: String,
    },
    /// Existing matching invocation was retained without launching a duplicate.
    Idempotent {
        /// Identity of the already recorded invocation.
        wake_id: String,
    },
}

// ── B109 wake/nudge 富输入来源（纯解析契约）────────────────────────────────
// design/10 §4 + r46/B109 任务卡。三来源（explicit/file/stdin）互斥；
// 全 None 回退 default；Some("") 是显式空消息（不回退 default）。
// 解析器只处理已读入内存的 String 内容——文件读取与 UTF-8 校验在调用方
// （CLI）完成，失败则非零退出，不把非法字节带进解析器。
// messageBytes = text.as_bytes().len()（UTF-8 字节数，非 char 数）。

/// Message provenance retained by the caller; resolving a message writes no events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSource {
    /// Text supplied directly in the command.
    Explicit,
    /// UTF-8 contents read from the requested file.
    File,
    /// Text consumed from standard input.
    Stdin,
    /// Caller default used only when no explicit source was supplied.
    Default,
}

impl MessageSource {
    /// Stable provenance spelling used in persisted invocation facts.
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageSource::Explicit => "explicit",
            MessageSource::File => "file",
            MessageSource::Stdin => "stdin",
            MessageSource::Default => "default",
        }
    }
}

impl std::fmt::Display for MessageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 显式调用共用的富输入载体。三个来源字段各为已读入内存的 String；
/// None 表示该来源未给出，Some(s) 表示该来源给出（哪怕 s 为空）。
/// 构造见 [resolve_message_input]；CLI 层负责把 --message/-file/-stdin
/// 读成 String 后填入对应字段（非法 UTF-8/读失败在 CLI 非零退出，不进解析器）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageInput {
    /// Direct argument, including an explicitly empty string.
    pub explicit: Option<String>,
    /// Already decoded UTF-8 file contents.
    pub file_contents: Option<String>,
    /// Already read standard input contents.
    pub stdin_contents: Option<String>,
}

impl MessageInput {
    /// 显式文本来源快捷构造。
    pub fn from_explicit(text: impl Into<String>) -> Self {
        MessageInput {
            explicit: Some(text.into()),
            file_contents: None,
            stdin_contents: None,
        }
    }

    /// default 快捷构造（三来源全 None）。
    pub fn none() -> Self {
        MessageInput::default()
    }
}

/// 解析后的消息：最终文本 + 来源标记 + UTF-8 字节数。
/// `bytes == text.as_bytes().len()`，断言见 [resolve_message_input]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMessage {
    /// Exact text delivered by the mutually exclusive source selection.
    pub text: String,
    /// Selected source; an explicit empty message is never Default.
    pub source: MessageSource,
    /// UTF-8 byte length of text, not a character or token count.
    pub bytes: usize,
}

/// 纯解析：按互斥语义解析三来源。不进行任何 IO。
///
/// 规则（与种子契约一致）：
/// - 全 None → 回退 default_text，source=Default；
/// - 恰一个 Some → 取该字段文本（哪怕空串），source 对应 Explicit/File/Stdin；
/// - 多于一个 Some → Err（来源互斥冲突）。
///
/// `bytes` 恒等于 `text.as_bytes().len()`（UTF-8 字节数，非字符数）。
pub fn resolve_message_input(
    input: MessageInput,
    default_text: &str,
) -> Result<ResolvedMessage, String> {
    let mut present: Vec<(&str, &str, MessageSource)> = Vec::new();
    if let Some(s) = input.explicit.as_deref() {
        present.push(("explicit", s, MessageSource::Explicit));
    }
    if let Some(s) = input.file_contents.as_deref() {
        present.push(("file", s, MessageSource::File));
    }
    if let Some(s) = input.stdin_contents.as_deref() {
        present.push(("stdin", s, MessageSource::Stdin));
    }
    match present.len() {
        0 => {
            let text = default_text.to_string();
            let bytes = text.as_bytes().len();
            Ok(ResolvedMessage {
                text,
                source: MessageSource::Default,
                bytes,
            })
        }
        1 => {
            let (_label, text, source) = present.pop().expect("len==1");
            let text = text.to_string();
            let bytes = text.as_bytes().len();
            Ok(ResolvedMessage {
                text,
                source,
                bytes,
            })
        }
        _ => {
            let labels: Vec<&str> = present.iter().map(|(l, _, _)| *l).collect();
            Err(format!(
                "wake/nudge 消息来源互斥冲突：同时给出 [{}]；请仅使用其中之一",
                labels.join(", ")
            ))
        }
    }
}


fn legacy_wake_supervisor_launch_variant() -> WakeSupervisorLaunchVariant {
    WakeSupervisorLaunchVariant::LegacyRevision2
}

fn ordinary_managed_wake_runtime_limit() -> ManagedWakeRuntimeLimit {
    managed_wake_runtime_limit(None)
}

fn false_value(value: &bool) -> bool {
    !*value
}

fn default_wake_supervisor_protocol_revision() -> u32 {
    LEGACY_WAKE_SUPERVISOR_PROTOCOL_REVISION
}


/// Ledger-safe subset of immutable managed-wake terminal facts used by the
/// outcome classifier.  Deliberately excluding elapsed time and signals from
/// the decision surface makes both of them observational evidence only.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWakeTerminationFacts {
    pub wake_id: String,
    pub agent: String,
    pub completion_reason: Option<String>,
    pub terminal_seen: bool,
    pub exited_naturally: bool,
    pub hard_deadline_reached: bool,
    pub cancel_request_id: Option<String>,
    pub signals: Vec<String>,
    pub managed_scope_terminated: bool,
    pub log_bytes_read: u64,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedWakeOutcomeClass {
    DeliveredTerminal,
    TruncatedNoTerminal,
    StoppedByHardDeadline,
    StoppedByAuthenticatedCancel,
    OperationalError(String),
}

impl ManagedWakeOutcomeClass {
    fn ledger_name(&self) -> &'static str {
        match self {
            Self::DeliveredTerminal => "DeliveredTerminal",
            Self::TruncatedNoTerminal => "TruncatedNoTerminal",
            Self::StoppedByHardDeadline => "StoppedByHardDeadline",
            Self::StoppedByAuthenticatedCancel => "StoppedByAuthenticatedCancel",
            Self::OperationalError(_) => "OperationalError",
        }
    }
}

/// Classify only authenticated terminal facts.  `signals`, elapsed time,
/// natural-exit timing and log volume are intentionally not consulted: they
/// cannot prove why a provider stopped.
#[doc(hidden)]
pub fn classify_managed_wake_outcome(
    facts: &ManagedWakeTerminationFacts,
) -> ManagedWakeOutcomeClass {
    if facts.wake_id.trim().is_empty() {
        return ManagedWakeOutcomeClass::OperationalError(
            "terminal status is missing wakeId".to_string(),
        );
    }
    if facts.agent.trim().is_empty() {
        return ManagedWakeOutcomeClass::OperationalError(
            "terminal status is missing agent".to_string(),
        );
    }
    let Some(completion_reason) = facts
        .completion_reason
        .as_deref()
        .filter(|reason| !reason.trim().is_empty())
    else {
        return ManagedWakeOutcomeClass::OperationalError(
            "terminal status is missing completionReason".to_string(),
        );
    };

    // Provider terminal evidence wins even when ordinary post-exit group
    // convergence recorded TERM/KILL signals.
    if facts.terminal_seen {
        return ManagedWakeOutcomeClass::DeliveredTerminal;
    }
    if facts.hard_deadline_reached {
        return ManagedWakeOutcomeClass::StoppedByHardDeadline;
    }
    if facts
        .cancel_request_id
        .as_deref()
        .is_some_and(|request_id| !request_id.trim().is_empty())
    {
        return ManagedWakeOutcomeClass::StoppedByAuthenticatedCancel;
    }
    if !facts.managed_scope_terminated {
        return ManagedWakeOutcomeClass::OperationalError(
            "managed scope termination is unproven".to_string(),
        );
    }

    match completion_reason {
        "natural-exit" => ManagedWakeOutcomeClass::TruncatedNoTerminal,
        "hard-deadline" => ManagedWakeOutcomeClass::OperationalError(
            "hard-deadline reason lacks hardDeadlineReached".to_string(),
        ),
        "manual-cancel" => ManagedWakeOutcomeClass::OperationalError(
            "manual-cancel reason lacks cancelRequestId".to_string(),
        ),
        "exact-terminal" => ManagedWakeOutcomeClass::OperationalError(
            "exact-terminal reason lacks terminalSeen".to_string(),
        ),
        "operational-error" => ManagedWakeOutcomeClass::OperationalError(
            "managed wake supervisor reported operational-error".to_string(),
        ),
        other => ManagedWakeOutcomeClass::OperationalError(format!(
            "unsupported completionReason {other:?}"
        )),
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedWakeCancelDisposition {
    /// The custodian acknowledged this request; cleanup may still be running.
    Accepted,
    /// An authenticated earlier request already owns cancellation.
    AlreadyCanceling,
    /// A terminal receipt existed before this request was accepted.
    AlreadyTerminal,
    /// Terminal or cleanup intent exists without complete owned-scope closure.
    CleanupPending,
    /// Request was durably published but no acknowledgement arrived within the wait.
    RequestPending,
}

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWakeCancelResult {
    /// Whether the authenticated request is pending, accepted or already terminal.
    pub disposition: ManagedWakeCancelDisposition,
    /// Exact invocation addressed by the request.
    pub wake_id: String,
    /// Identity of the durable first-writer cancellation request.
    pub request_id: String,
    /// Validated operator reason retained for audit.
    pub reason: String,
    /// Authenticated winning stop reason, if the supervisor supplied one.
    pub completion_reason: Option<ManagedWakeStopReason>,
    /// Only true with recorded owned-process termination; request delivery alone is insufficient.
    pub managed_scope_terminated: bool,
}

fn with_wake_identity_lock<T>(
    root: &Path,
    agent: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    validate_agent_component(agent)?;
    let lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&lock_dir).context("create wake identity lock directory failed")?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_dir.join(format!("wake-identity-{agent}.lock")))
        .context("open wake identity lock failed")?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            bail!("wake identity lease busy for {agent}: fail-fast")
        }
        Err(error) => return Err(error).context("acquire wake identity lock failed"),
    };
    action()
}

fn fresh_uuid() -> String {
    let mut bytes = ulid::Ulid::new().to_bytes();
    // RFC 4122 version/variant bits；随机源沿用仓内已有 ulid，无新增依赖。
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Generate the canonical control identity used by every managed wake entry.
#[doc(hidden)]
pub fn fresh_managed_wake_id() -> String {
    fresh_uuid()
}

fn read_managed_wake_control_descriptor(
    root: &Path,
    wake_id: &str,
) -> Result<(PathBuf, ManagedWakeControlDescriptor)> {
    if !is_strict_uuid(wake_id) {
        bail!("managed wake cancel wakeId must be a strict UUID");
    }
    let dir = authenticated_supervisor_dir(root, false)?;
    let path = dir.join(format!("{wake_id}.control.json"));
    if !path.exists() {
        bail!("unsupported-or-not-managed: no managed control descriptor for wakeId");
    }
    let body = secure_read_lf_frame(&path, &dir, MAX_MANAGED_WAKE_CONTROL_BYTES)?;
    let descriptor: ManagedWakeControlDescriptor =
        serde_json::from_slice(&body).context("parse managed wake control descriptor failed")?;
    match (descriptor.version, descriptor.invocation.as_ref()) {
        (MANAGED_WAKE_CONTROL_VERSION, None) => {}
        (MANAGED_WAKE_DESCRIPTOR_VERSION, Some(facts)) => validate_invocation_facts(root, facts)?,
        _ => bail!("unsupported managed descriptor version/invocation binding"),
    }
    if descriptor.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
        || descriptor.wake_id != wake_id
        || !is_lower_hex_token(&descriptor.token)
        || descriptor.supervisor_pid == 0
        || descriptor.provider_pid == 0
        || descriptor.runtime_limit
            != managed_wake_runtime_limit(descriptor.runtime_limit.requested_review_deadline_secs)
    {
        bail!("managed wake control descriptor identity/protocol/runtime mismatch");
    }
    match (
        descriptor.attach_action_id.as_deref(),
        descriptor.continued_from_wake_id.as_deref(),
    ) {
        (None, None) => {}
        (Some(action_id), Some(source_wake_id))
            if action_id.len() == 64
                && action_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && is_strict_uuid(source_wake_id)
                && source_wake_id != wake_id => {}
        _ => bail!("managed wake control descriptor attach binding mismatch"),
    }
    let expected_stop = dir.join(format!("{}.stop.json", descriptor.token));
    let expected_status = dir.join(format!("{}.status.json", descriptor.token));
    let expected_cancel = dir.join(format!("{}.cancel.json", descriptor.token));
    let expected_cancel_ack = dir.join(format!("{}.cancel-ack.json", descriptor.token));
    if descriptor.stop_path != expected_stop
        || descriptor.status_path != expected_status
        || descriptor.cancel_path != expected_cancel
        || descriptor.cancel_ack_path != expected_cancel_ack
    {
        bail!("managed wake control descriptor contains non-canonical paths");
    }
    Ok((dir, descriptor))
}

/// Durably publish or follow the canonical first-writer cancel request. This
/// function never signals a process; only the authenticated hidden supervisor
/// can turn the request into the existing receipt-checked cleanup ladder.
#[doc(hidden)]
pub fn request_managed_wake_cancel(
    root: &Path,
    wake_id: &str,
    reason: &str,
) -> Result<ManagedWakeCancelResult> {
    validate_cancel_reason(reason).map_err(anyhow::Error::msg)?;
    let (dir, descriptor) = read_managed_wake_control_descriptor(root, wake_id)?;
    let proposed = ManagedWakeCancelRequest {
        version: MANAGED_WAKE_CONTROL_VERSION,
        wake_id: wake_id.to_string(),
        token: descriptor.token.clone(),
        request_id: fresh_managed_wake_id(),
        reason: reason.to_string(),
    };
    let mut created = false;
    let request = match atomic_create_json(&descriptor.cancel_path, &proposed) {
        Ok(()) => {
            created = true;
            proposed
        }
        Err(install_error) if descriptor.cancel_path.exists() => {
            let body =
                secure_read_lf_frame(&descriptor.cancel_path, &dir, MAX_MANAGED_WAKE_CANCEL_BYTES)?;
            parse_managed_wake_cancel_request(&body, wake_id, &descriptor.token)
                .map_err(|error| anyhow::anyhow!(
                    "canonical managed wake cancel request is invalid after no-replace race: {error}; install={install_error:#}"
                ))?
        }
        Err(error) => return Err(error),
    };

    let wait_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if descriptor.status_path.exists() {
            let body = secure_read_lf_frame(
                &descriptor.status_path,
                &dir,
                MAX_MANAGED_WAKE_CONTROL_BYTES,
            )?;
            let status: WakeSupervisorStatus =
                serde_json::from_slice(&body).context("parse final managed wake status failed")?;
            if status.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
                || status.token != descriptor.token
                || status.wake_id != wake_id
                || status.runtime_limit != descriptor.runtime_limit
            {
                bail!("final managed wake status identity/protocol/runtime mismatch");
            }
            return Ok(ManagedWakeCancelResult {
                disposition: if status.managed_scope_terminated && status.error.is_none() {
                    ManagedWakeCancelDisposition::AlreadyTerminal
                } else {
                    ManagedWakeCancelDisposition::CleanupPending
                },
                wake_id: wake_id.to_string(),
                request_id: request.request_id,
                reason: request.reason,
                completion_reason: Some(status.completion_reason),
                managed_scope_terminated: status.managed_scope_terminated && status.error.is_none(),
            });
        }
        if descriptor.cancel_ack_path.exists() {
            let body = secure_read_lf_frame(
                &descriptor.cancel_ack_path,
                &dir,
                MAX_MANAGED_WAKE_CONTROL_BYTES,
            )?;
            let ack: ManagedWakeCancelAck =
                serde_json::from_slice(&body).context("parse managed wake cancel ACK failed")?;
            if ack.version != MANAGED_WAKE_CONTROL_VERSION
                || ack.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
                || ack.token != descriptor.token
                || ack.wake_id != wake_id
                || ack.request_id != request.request_id
                || !matches!(ack.state.as_str(), "accepted" | "already-terminal")
            {
                bail!("managed wake cancel ACK identity/protocol/state mismatch");
            }
            return Ok(ManagedWakeCancelResult {
                disposition: if ack.state == "already-terminal" {
                    ManagedWakeCancelDisposition::CleanupPending
                } else if created {
                    ManagedWakeCancelDisposition::Accepted
                } else {
                    ManagedWakeCancelDisposition::AlreadyCanceling
                },
                wake_id: wake_id.to_string(),
                request_id: request.request_id,
                reason: request.reason,
                completion_reason: Some(ack.winner),
                managed_scope_terminated: false,
            });
        }
        if descriptor.stop_path.exists() {
            let body =
                secure_read_lf_frame(&descriptor.stop_path, &dir, MAX_MANAGED_WAKE_CONTROL_BYTES)?;
            let stop: ManagedWakeStopReceipt =
                serde_json::from_slice(&body).context("parse managed wake stop receipt failed")?;
            if stop.version != MANAGED_WAKE_CONTROL_VERSION
                || stop.protocol_revision != WAKE_SUPERVISOR_PROTOCOL_REVISION
                || stop.token != descriptor.token
                || stop.wake_id != wake_id
            {
                bail!("managed wake stop receipt identity/protocol mismatch");
            }
            return Ok(ManagedWakeCancelResult {
                disposition: ManagedWakeCancelDisposition::CleanupPending,
                wake_id: wake_id.to_string(),
                request_id: request.request_id,
                reason: request.reason,
                completion_reason: Some(stop.completion_reason),
                managed_scope_terminated: false,
            });
        }
        if Instant::now() >= wait_deadline {
            return Ok(ManagedWakeCancelResult {
                disposition: ManagedWakeCancelDisposition::RequestPending,
                wake_id: wake_id.to_string(),
                request_id: request.request_id,
                reason: request.reason,
                completion_reason: None,
                managed_scope_terminated: false,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read-only projection of authenticated control artifacts, with secrets omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedWakeStatusView {
    /// Immutable managed invocation identity.
    pub wake_id: String,
    /// Recorded harness alias; never re-resolved from current configuration.
    pub agent: String,
    /// Recorded driver spelling, not a claimed upstream provider.
    pub provider_kind: String,
    /// Whether an authenticated controller terminal record exists.
    pub terminal: bool,
    /// Whether the controller proved its owned process scope ended; persistent native jobs need separate proof.
    pub managed_scope_terminated: bool,
    /// Recorded physical stop reason when available.
    pub completion_reason: Option<String>,
    /// Classification of authenticated terminal facts; absence remains unknown.
    pub outcome: Option<String>,
    /// Whether a driver session receipt was authenticated.
    pub session_present: bool,
    /// Receipt availability or validation state, not inferred provider liveness.
    pub session_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Digest of the authenticated driver session identity; never its credentials.
    pub session_digest: Option<String>,
}

fn read_authenticated_terminal_status(
    dir: &Path,
    descriptor: &ManagedWakeControlDescriptor,
) -> Result<Option<WakeSupervisorStatus>> {
    if !descriptor.status_path.exists() {
        return Ok(None);
    }
    let body = secure_read_lf_frame(&descriptor.status_path, dir, MAX_MANAGED_WAKE_CONTROL_BYTES)?;
    let status: WakeSupervisorStatus = serde_json::from_slice(&body)
        .context("parse immutable managed wake terminal status failed")?;
    if status.protocol_revision != descriptor.protocol_revision
        || status.token != descriptor.token
        || status.wake_id != descriptor.wake_id
        || status.runtime_limit != descriptor.runtime_limit
        || status.agent.as_deref().is_none_or(str::is_empty)
    {
        bail!("immutable managed wake terminal status identity/protocol mismatch");
    }
    Ok(Some(status))
}

fn termination_facts_from_status(status: &WakeSupervisorStatus) -> ManagedWakeTerminationFacts {
    ManagedWakeTerminationFacts {
        wake_id: status.wake_id.clone(),
        agent: status.agent.clone().unwrap_or_default(),
        completion_reason: Some(status.completion_reason.ledger_name().to_string()),
        terminal_seen: status.terminal_seen,
        exited_naturally: status.exited_naturally,
        hard_deadline_reached: status.hard_deadline_reached,
        cancel_request_id: status.cancel_request_id.clone(),
        signals: status.signals.clone(),
        managed_scope_terminated: status.managed_scope_terminated,
        log_bytes_read: status.log_bytes_read,
    }
}

const CHANNEL_ONE_SHOT_DEFAULT_SECS: u64 = 7_200;

fn channel_binding_from_rendered(rendered: &super::RenderedInvocationV1, standalone: bool) -> ChannelSupervisorBindingV1 {
    let prepared = rendered.prepared();
    ChannelSupervisorBindingV1 {
        standalone,
        requested_tuple: Some(prepared.requested().tuple.clone()),
        effective_tuple: Some(prepared.effective().clone()),
        alias: prepared.alias().to_owned(),
        driver: prepared.driver(),
        action: prepared.requested().action.as_str().to_owned(),
        config_digest: prepared.config_digest().to_owned(),
        request_digest: prepared.request_digest().to_owned(),
        attachment_manifest_digest: prepared.attachment_manifest_digest().to_owned(),
        command_digest: rendered.command_digest().to_owned(),
        program_identity_digest: rendered.program_identity_digest().to_owned(),
        configured_executable: prepared.executable().to_path_buf(),
        configured_executable_identity_digest: prepared.executable_identity().sha256().to_owned(),
        wrapper_executable: rendered.wrapper_identity().map(|identity| identity.configured_path().to_path_buf()),
        wrapper_identity_digest: rendered.wrapper_identity().map(|identity| identity.sha256().to_owned()),
        head_root: if prepared.target_worktree().as_os_str().is_empty() {
            prepared.project_root().to_path_buf()
        } else {
            prepared.target_worktree().to_path_buf()
        },
        fixed_head: prepared.target_head().to_owned(),
    }
}

/// Start one explicitly addressed managed invocation in a standalone Git project.
/// The same immutable channel and detached custodian are used as by selfhost;
/// no task, round, lease, policy registry or protocol ledger is created.
pub fn run_direct_wake(
    root: &Path,
    alias: &str,
    resolved: ResolvedMessage,
    deadline_secs: Option<u64>,
) -> Result<WakeRunOutcome> {
    with_wake_identity_lock(root, alias, || {
        if crate::has_selfhost_state(root)? {
            bail!("direct wake refuses selfhost state; use the selfhost task entry");
        }
        let root = fs::canonicalize(root)?;
        let head = crate::gitx::rev_parse(&root, "HEAD^{commit}")?;
        let snapshot = crate::harness_config::load_harness_config_snapshot(&root)?;
        let wake_id = fresh_managed_wake_id();
        let prepared = super::prepare_invocation(&snapshot, super::InvocationRequest {
            alias: alias.to_owned(),
            action: super::InvocationAction::Consult,
            prompt: resolved.text,
            project_root: root.clone(),
            target_worktree: root.clone(),
            target_head: head,
            attachments: super::capture_attachment_manifest_v1(&[])?,
        })?;
        let deadline = prepared.limits().deadline_seconds(
            deadline_secs, CHANNEL_ONE_SHOT_DEFAULT_SECS, MANAGED_WAKE_MAX_RUNTIME_SECS,
        )?;
        let rendered = super::render_invocation_v1(prepared, super::InvocationContextV1 {
            action_id: wake_id.clone(),
            wake_id: wake_id.clone(),
            // Existing consult envelope placeholders are transport context,
            // never RoundOpened/Task/attempt facts or a new coordination entity.
            round: "manual".to_owned(),
            task_id: "CONSULT".to_owned(),
            attempt_id: "CONSULT-A0000".to_owned(),
            review_output: None,
            orch_executable: std::env::current_exe()?,
            deadline_secs: deadline,
        })?;
        let rendered = super::preflight_invocation_v1(rendered)?.into_rendered();
        fs::create_dir_all(root.join("coordination/runtime/logs"))?;
        let log_path = crate::activity::wake_log_path(&root, alias);
        File::options().write(true).create_new(true).open(&log_path)?;
        let binding = channel_binding_from_rendered(&rendered, true);
        spawn_channel_wake_supervisor(&root, alias, rendered.argv(), &log_path, &wake_id,
            managed_wake_runtime_limit(Some(deadline)), rendered.env(), rendered.cwd(), binding)?;
        Ok(WakeRunOutcome::Spawned { wake_id })
    })
}

/// Identify a direct invocation from its authenticated immutable descriptor.
/// This reads recorded identity, never the current harness configuration.
pub fn is_direct_wake(root: &Path, wake_id: &str) -> Result<bool> {
    let (_, descriptor) = read_managed_wake_control_descriptor(root, wake_id)?;
    Ok(descriptor.invocation.as_ref().is_some_and(|facts| facts.binding.standalone))
}

fn status_from_artifacts(
    dir: &Path,
    descriptor: &ManagedWakeControlDescriptor,
    agent: &str,
    provider_kind: &str,
) -> Result<ManagedWakeStatusView> {
    let wake_id = descriptor.wake_id.as_str();
    let status = read_authenticated_terminal_status(dir, descriptor)?;
    if status.as_ref().and_then(|status| status.agent.as_deref()).is_some_and(|observed| observed != agent) {
        bail!("untrusted-runtime-artifact: terminal status agent binding mismatch");
    }
    let session = if provider_kind == "opencode" {
        let session_path = dir.join(format!("{}.session.json", descriptor.token));
        if session_path.exists() {
            Some(read_opencode_session_receipt_file(&session_path, dir, wake_id, &descriptor.token, agent)?)
        } else { None }
    } else { None };
    let facts = status.as_ref().map(termination_facts_from_status);
    Ok(ManagedWakeStatusView {
        wake_id: wake_id.to_owned(),
        agent: agent.to_owned(),
        provider_kind: provider_kind.to_owned(),
        terminal: status.is_some(),
        managed_scope_terminated: status.as_ref().is_some_and(|status| status.managed_scope_terminated),
        completion_reason: status.as_ref().map(|status| status.completion_reason.ledger_name().to_owned()),
        outcome: facts.as_ref().map(classify_managed_wake_outcome).map(|outcome| outcome.ledger_name().to_owned()),
        session_present: session.is_some(),
        session_status: if session.is_some() { "available" } else { "session-unavailable" }.to_owned(),
        session_digest: session.map(|receipt| receipt.session_digest),
    })
}

/// Read the existing supported status view using captured standalone identity.
/// A controller terminal is not by itself proof that a persistent native job ended.
pub fn direct_wake_status(root: &Path, wake_id: &str) -> Result<ManagedWakeStatusView> {
    let (dir, descriptor) = read_managed_wake_control_descriptor(root, wake_id)?;
    let facts = descriptor.invocation.as_ref().filter(|facts| facts.binding.standalone)
        .context("legacy/selfhost descriptor requires the selfhost status entry")?;
    let contract = channel_binding_driver_contract(&facts.binding)?;
    if !contract.control.status { bail!("unsupported: driver does not provide managed status"); }
    status_from_artifacts(&dir, &descriptor, &facts.binding.alias, facts.binding.driver.as_str())
}

// Enumerate only inside the private authentication boundary. No raw entry name or
// verification error is returned, because sidecar names contain credentials.
pub(crate) fn observation_projection(root: &Path, remaining: &mut usize) -> (Vec<serde_json::Value>, Vec<String>, bool) {
    let mut rows=Vec::new(); let mut diagnostics=Vec::new(); let mut truncated=false;
    if !root.join("coordination/runtime/supervisors").try_exists().unwrap_or(true) {return (rows,diagnostics,false);}
    let dir=match authenticated_supervisor_dir(root,false) {Ok(d)=>d,Err(_)=>return (rows,vec!["supervisor directory unavailable".into()],false)};
    let entries=match fs::read_dir(&dir) {Ok(e)=>e,Err(_)=>return (rows,vec!["supervisor enumeration unavailable".into()],false)};
    let mut ids=std::collections::BTreeSet::new();
    for entry in entries {
        let Ok(entry)=entry else {diagnostics.push("supervisor entry unavailable".into());continue};
        let name=entry.file_name();let Some(name)=name.to_str() else {continue};
        if let Some(id)=name.strip_suffix(".control.json") {
            if is_strict_uuid(id) {ids.insert(id.to_owned());if ids.len()>1024 {ids.pop_first();truncated=true;}}
            else if diagnostics.is_empty(){diagnostics.push("unknown supervisor control entry".into());}
        }
    }
    for id in ids.into_iter().rev() {
        // Reserve bounded descriptor and status/session frames before existing readers.
        const RESERVATION:usize=256*1024;
        if *remaining<RESERVATION {truncated=true;break;} *remaining-=RESERVATION;
        let projected=(||->Result<Option<serde_json::Value>> {
            let (dir,descriptor)=read_managed_wake_control_descriptor(root,&id)?;
            let Some(facts)=descriptor.invocation.as_ref().filter(|f|f.binding.standalone) else {return Ok(None)};
            let contract=channel_binding_driver_contract(&facts.binding)?;
            let status=if contract.control.status {status_from_artifacts(&dir,&descriptor,&facts.binding.alias,facts.binding.driver.as_str()).ok()} else {None};
            Ok(Some(serde_json::json!({"wakeId":id,"publishedAt":facts.published_at,"alias":facts.binding.alias,
                "driver":facts.binding.driver.as_str(),"action":facts.binding.action,"requestedTuple":facts.binding.requested_tuple,
                "effectiveTuple":facts.binding.effective_tuple,"fixedHead":facts.binding.fixed_head,"parameters":{"configDigest":facts.binding.config_digest,"runtimeLimit":descriptor.runtime_limit},"statusSupported":contract.control.status,"status":status})))
        })();
        match projected {Ok(Some(v))=>rows.push(v),Ok(None)=>{},Err(_)=>{if diagnostics.len()<8 {diagnostics.push("supervisor observation unavailable".into());}}}
    }
    if truncated {diagnostics.push("supervisor candidates or metadata budget clipped".into());}
    (rows,diagnostics,truncated)
}

/// Request the existing authenticated custodian cancellation for one direct wake.
/// The request does not signal processes or certify termination of a persistent backend.
pub fn cancel_direct_wake(root: &Path, wake_id: &str, reason: &str) -> Result<ManagedWakeCancelResult> {
    if !is_direct_wake(root, wake_id)? { bail!("selfhost descriptor requires the selfhost cancel entry"); }
    request_managed_wake_cancel(root, wake_id, reason)
}


#[cfg(test)]
std::thread_local! {
    static B177_FORGED_ACK_PHASE: std::cell::Cell<Option<B177ForgedAckPhase>> =
        std::cell::Cell::new(None);
    static B177_FORGED_ACK_CAPTURE: std::cell::RefCell<Option<B177ForgedAckCapture>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
mod descriptor_compatibility_tests {
    use super::*;

    fn facts(root: &Path) -> ManagedInvocationFactsV1 {
        let tuple = super::super::InvocationTuple {
            provider: Some("local".into()), model: Some("fixture-model".into()),
            effort: None, mode: None,
        };
        ManagedInvocationFactsV1 {
            binding: ChannelSupervisorBindingV1 {
                standalone: true, requested_tuple: Some(tuple.clone()), effective_tuple: Some(tuple),
                alias: "fixture".into(), driver: crate::harness::HarnessId::OpenCode,
                action: "consult".into(), config_digest: "a".repeat(64), request_digest: "b".repeat(64),
                attachment_manifest_digest: "c".repeat(64), command_digest: "d".repeat(64),
                program_identity_digest: "e".repeat(64), configured_executable: root.join("opencode"),
                configured_executable_identity_digest: "f".repeat(64), wrapper_executable: None,
                wrapper_identity_digest: None, head_root: root.to_path_buf(), fixed_head: "a".repeat(40),
            },
            cwd: root.to_path_buf(), log_path: root.join("coordination/runtime/logs/fixture.log"),
            published_at: "2026-09-07T00:00:00Z".into(),
        }
    }

    #[test]
    fn version_one_remains_readable_and_version_two_binds_nonsecret_invocation() {
        let root = crate::util::test_scratch_dir("b330-control-compatibility");
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let dir = authenticated_supervisor_dir(&root, true).unwrap();
        let wake = "019fc320-1111-4222-8333-444455556666";
        let token = "a".repeat(32);
        let path = dir.join(format!("{wake}.control.json"));
        for invocation in [None, Some(facts(&root))] {
            let descriptor = build_control_descriptor(&root, wake, &token, 1, 2,
                &dir.join(format!("{token}.status.json")), managed_wake_runtime_limit(None),
                None, invocation).unwrap();
            atomic_create_json(&path, &descriptor).unwrap();
            assert_eq!(read_managed_wake_control_descriptor(&root, wake).unwrap().1, descriptor);
            let body = fs::read_to_string(&path).unwrap();
            assert!(!body.contains("\"env\"") && !body.contains("\"argv\""));
            assert!(request_managed_wake_cancel(&root, "not-a-uuid", "fixture").is_err());
            assert!(!dir.join(format!("{token}.cancel.json")).exists());
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_version_binding_and_paths_cannot_create_control_requests() {
        let root = crate::util::test_scratch_dir("b330-control-invalid");
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let dir = authenticated_supervisor_dir(&root, true).unwrap();
        let wake = "019fc320-1111-4222-8333-444455556666";
        let token = "a".repeat(32);
        let path = dir.join(format!("{wake}.control.json"));
        let descriptor = build_control_descriptor(&root, wake, &token, 1, 2,
            &dir.join(format!("{token}.status.json")), managed_wake_runtime_limit(None),
            None, Some(facts(&root))).unwrap();
        let original = serde_json::to_value(descriptor).unwrap();
        for case in 0..8 {
            let mut value = original.clone();
            match case {
                0 => value["version"] = 3.into(),
                1 => { value.as_object_mut().unwrap().remove("version"); },
                2 => value["version"] = 1.into(),
                3 => { value.as_object_mut().unwrap().remove("invocation"); },
                4 => value["invocation"]["binding"]["requestedTuple"] = serde_json::Value::Null,
                5 => value["invocation"]["binding"]["configDigest"] = "bad-digest".into(),
                6 => value["invocation"]["cwd"] = "/wrong-project".into(),
                7 => value["statusPath"] = "/wrong-status".into(),
                _ => unreachable!(),
            }
            atomic_create_json(&path, &value).unwrap();
            assert!(read_managed_wake_control_descriptor(&root, wake).is_err(), "case {case}");
            assert!(request_managed_wake_cancel(&root, wake, "fixture").is_err(), "case {case}");
            assert!(!dir.join(format!("{token}.cancel.json")).exists(), "case {case}");
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}

// Nest the retained task layer beneath its transport so it can use the same
// private kernel without widening implementation helpers into public APIs.
#[cfg(feature = "selfhost")]
#[path = "../wake.rs"]
pub mod selfhost;

#[cfg(test)]
mod r93_initial_identity_tests {
    use super::*;

    #[test]
    fn initial_offer_capture_cannot_publish_a_transient_shell_identity() {
        let mut command = Command::new("/bin/bash");
        command.args(["-c", "/bin/sleep 0.15; exec /bin/sleep 2"]);
        configure_isolated_session(&mut command);
        let mut child = command.spawn().unwrap();
        let transient = executable_summary(Path::new("/bin/bash")).unwrap();
        let captured = capture_provider_credential_in_window(&mut ProcessObserver::default(), child.id(), Some(&transient));
        let expected = executable_summary(Path::new("/bin/sleep")).unwrap();
        let waited = child.wait().unwrap();
        assert!(waited.success());
        assert_eq!(captured.unwrap().executable_summary, expected,
            "initial OFFER must not publish the transient interpreter before a delayed same-PID exec");
    }
}

#[cfg(test)]
mod r93_short_provider_tests {
    use super::*;
    #[test]
    fn stable_short_lived_provider_does_not_wait_the_entire_startup_window() {
        let mut command=Command::new("/bin/sleep"); command.arg("0.35");
        configure_isolated_session(&mut command);
        let mut child=command.spawn().unwrap();
        let captured=capture_initial_provider_credential(&mut ProcessObserver::default(),child.id());
        let status=child.wait().unwrap();assert!(status.success());
        assert_eq!(captured.unwrap().pid,child.id());
    }
}

#[cfg(test)]
mod r93_revalidation_tests {
    use super::*;
    #[test]
    fn missing_second_census_never_authorizes_revalidation() {
        let mut command=Command::new("/bin/sleep");command.arg("1");configure_isolated_session(&mut command);
        let mut child=command.spawn().unwrap();
        let mut observer=ProcessObserver::default();observer.fail_topology_at=Some(2);
        let captured=sample_provider_credential(&mut observer,child.id());
        assert!(child.wait().unwrap().success());
        assert!(captured.unwrap_err().to_string().contains("injected process-topology snapshot failure"));
    }
}
