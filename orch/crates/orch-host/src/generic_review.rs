//! Policy-free schema-3 generic review lifecycle and artifact truth.
//!
//! Live actions consume the unified harness channel. The sole pre-channel
//! B319 decoder is private, recorded-history-only, and cannot spawn work.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Wire role shared by all schema-3 review requests.
pub const GENERIC_REVIEW_ROLE: &str = "review";

/// Version anchor for the policy-free schema-3 generic-review live contract.
pub const GENERIC_REVIEW_LIVE_CONTRACT_V1: u32 = 1;

/// Exact live identity shared by one generic request, terminal, and artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GenericReviewLiveIdentityV1 {
    task: String,
    attempt: String,
    harness: String,
    wake_id: String,
}

impl GenericReviewLiveIdentityV1 {
    /// Construct one exact `(task, attempt, harness, wakeId)` identity.
    pub fn new(task: &str, attempt: &str, harness: &str, wake_id: &str) -> Result<Self> {
        crate::card::validate_task_id(task)?;
        let attempt_suffix = attempt.strip_prefix(&format!("{task}-A"));
        if attempt_suffix.is_none_or(|suffix| {
            suffix.len() != 4
                || !suffix.bytes().all(|byte| byte.is_ascii_digit())
                || suffix == "0000"
        }) {
            bail!("generic review live attempt 必须是 task-A0001+ 四位 identity");
        }
        for (label, value) in [("harness", harness), ("wakeId", wake_id)] {
            if value.is_empty()
                || !value
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_alphanumeric())
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                bail!("generic review live {label} 必须是安全 identity component");
            }
        }
        Ok(Self {
            task: task.to_string(),
            attempt: attempt.to_string(),
            harness: harness.to_string(),
            wake_id: wake_id.to_string(),
        })
    }

    /// Return the only schema-3 review role; aliases never become role slots.
    pub const fn role(&self) -> &'static str {
        GENERIC_REVIEW_ROLE
    }
}

type GenericReviewIdentity = GenericReviewLiveIdentityV1;

/// Immutable request facts authenticated before a generic-review spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericReviewLiveBindingV1 {
    identity: GenericReviewLiveIdentityV1,
    reviewed_head: String,
    cwd: String,
    request_digest: String,
    config_digest: String,
    attachment_manifest_digest: String,
}

impl GenericReviewLiveBindingV1 {
    /// Bind one identity to fixed HEAD, cwd, request, config, and attachments.
    pub fn new(
        identity: GenericReviewLiveIdentityV1,
        reviewed_head: &str,
        cwd: &str,
        request_digest: &str,
        config_digest: &str,
        attachment_manifest_digest: &str,
    ) -> Result<Self> {
        let cwd_path = Path::new(cwd);
        if !canonical_sha(reviewed_head, 40)
            || !canonical_sha(request_digest, 64)
            || !canonical_sha(config_digest, 64)
            || !canonical_sha(attachment_manifest_digest, 64)
            || !cwd_path.is_absolute()
            || cwd_path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            bail!("generic review live binding head/cwd/digest 非 canonical");
        }
        Ok(Self {
            identity,
            reviewed_head: reviewed_head.to_string(),
            cwd: cwd.to_string(),
            request_digest: request_digest.to_string(),
            config_digest: config_digest.to_string(),
            attachment_manifest_digest: attachment_manifest_digest.to_string(),
        })
    }

    /// Match every immutable request fact exactly without filesystem rereads.
    pub fn matches(
        &self,
        reviewed_head: &str,
        cwd: &str,
        request_digest: &str,
        config_digest: &str,
        attachment_manifest_digest: &str,
    ) -> bool {
        self.reviewed_head == reviewed_head
            && self.cwd == cwd
            && self.request_digest == request_digest
            && self.config_digest == config_digest
            && self.attachment_manifest_digest == attachment_manifest_digest
    }
}

/// Policy-neutral terminal class for one initiated generic review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericReviewLiveTerminalV1 {
    /// A trusted terminal and substantive final artifact were authenticated.
    Answered(GenericReviewLiveIdentityV1),
    /// The provider or channel ended with an operational failure.
    Failed(GenericReviewLiveIdentityV1),
    /// The authenticated deadline elapsed without an answer.
    TimedOut(GenericReviewLiveIdentityV1),
    /// The invocation ended without a substantive final answer.
    Empty(GenericReviewLiveIdentityV1),
    /// An authenticated cancellation closed the invocation.
    Canceled(GenericReviewLiveIdentityV1),
    /// An exact typed action rejection closed the initiated request.
    Rejected(GenericReviewLiveIdentityV1),
}

impl GenericReviewLiveTerminalV1 {
    fn identity(&self) -> &GenericReviewLiveIdentityV1 {
        match self {
            Self::Answered(identity)
            | Self::Failed(identity)
            | Self::TimedOut(identity)
            | Self::Empty(identity)
            | Self::Canceled(identity)
            | Self::Rejected(identity) => identity,
        }
    }
}

/// Return a collision-free mechanical key for one complete live identity.
pub fn generic_review_live_slot_key_v1(identity: &GenericReviewLiveIdentityV1) -> String {
    let mut key = String::from("generic-review-live-v1");
    for value in [
        identity.task.as_str(),
        identity.attempt.as_str(),
        identity.harness.as_str(),
        identity.wake_id.as_str(),
    ] {
        key.push(':');
        key.push_str(&value.len().to_string());
        key.push(':');
        key.push_str(value);
    }
    key
}

/// Require one and only one terminal class for every initiated request.
///
/// This function deliberately does not inspect review verdicts, count PASS
/// results, apply vetoes, choose a roster, or encode a quorum policy.
pub fn validate_generic_review_live_terminals_v1(
    requests: &[GenericReviewLiveIdentityV1],
    terminals: &[GenericReviewLiveTerminalV1],
) -> Result<()> {
    let request_set = requests.iter().cloned().collect::<BTreeSet<_>>();
    if request_set.len() != requests.len() {
        bail!("generic review live request identity 重复");
    }
    let mut terminal_set = BTreeSet::new();
    for terminal in terminals {
        let identity = terminal.identity();
        if !request_set.contains(identity) || !terminal_set.insert(identity.clone()) {
            bail!("generic review live terminal identity unknown/duplicate");
        }
    }
    if terminal_set != request_set {
        bail!("generic review live initiated request 尚未全部终态");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GenericReviewFrontmatter {
    task_id: String,
    round: String,
    attempt_id: String,
    role: String,
    reviewer: String,
    verdict: String,
    reviewed_head: String,
    wake_id: String,
}

#[derive(Debug)]
struct CheckedGenericArtifact {
    verdict: String,
    body_len: u64,
}

/// Result of installing and durably accounting one generic review artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericReviewDeliveryOutcome {
    /// Canonical tracked review path.
    pub path: String,
    /// Scoped main commit containing both artifact and `ReviewDelivered`.
    pub commit_sha: String,
    /// Durable delivery event identity.
    pub delivery_event_id: String,
    /// Whether the exact delivery was already committed.
    pub replayed: bool,
}

fn payload_string<'a>(event: &'a orch_core::EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
}

fn canonical_sha(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

const HISTORICAL_BOOTSTRAP_SOURCE_PATHS: &[&str] = &[
    "coordination/agents.yaml",
    "coordination/harnesses.yaml",
    "coordination/adapters",
    "coordination/tools",
    "coordination/scripts/wake-multica.sh",
    "orch/scripts/wake-pi-stream.sh",
    "orch/scripts/wake-dsh-stream.sh",
];

fn historical_bootstrap_source_digest(root: &Path, base_sha: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-r", "--full-tree", "-z", base_sha, "--"])
        .args(HISTORICAL_BOOTSTRAP_SOURCE_PATHS)
        .output()
        .context("historical bootstrap source census 失败")?;
    if !output.status.success() {
        bail!(
            "historical bootstrap ls-tree 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut paths = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let record = std::str::from_utf8(record).context("historical ls-tree record 非 UTF-8")?;
        let (header, path) = record
            .split_once('\t')
            .context("historical ls-tree record 缺 tab")?;
        let fields = header.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || !matches!(fields[0], "100644" | "100755") || fields[1] != "blob" {
            bail!("historical bootstrap source 非 regular blob: {record:?}");
        }
        paths.push(path.to_string());
    }
    paths.sort();
    if paths.is_empty() {
        bail!("historical bootstrap source census 为空");
    }
    let mut hasher = Sha256::new();
    hasher.update(b"orch-bootstrap-invocation-sources-v1\0");
    for path in paths {
        let bytes = crate::gitx::show_bytes(root, base_sha, &path)?;
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn split_frontmatter(bytes: &[u8]) -> Result<(&str, &str)> {
    let text = std::str::from_utf8(bytes).context("generic review artifact 必须为 UTF-8")?;
    let rest = text
        .strip_prefix("---\n")
        .context("generic review artifact 缺 opening frontmatter fence")?;
    let (frontmatter, body) = rest
        .split_once("\n---\n")
        .context("generic review artifact 缺 closing frontmatter fence")?;
    Ok((frontmatter, body))
}

fn check_artifact(
    bytes: &[u8],
    identity: &GenericReviewIdentity,
    round: &str,
    reviewed_head: &str,
) -> Result<CheckedGenericArtifact> {
    let (frontmatter, body) = split_frontmatter(bytes)?;
    let parsed: GenericReviewFrontmatter =
        serde_yaml::from_str(frontmatter).context("generic review frontmatter 非 closed schema")?;
    if parsed.task_id != identity.task
        || parsed.round != round
        || parsed.attempt_id != identity.attempt
        || parsed.role != GENERIC_REVIEW_ROLE
        || parsed.reviewer != identity.harness
        || parsed.wake_id != identity.wake_id
        || parsed.reviewed_head != reviewed_head
        || !canonical_sha(&parsed.reviewed_head, 40)
    {
        bail!("generic review frontmatter 与 request/fixed-HEAD tuple 不匹配");
    }
    if !matches!(parsed.verdict.as_str(), "PASS" | "FAIL" | "BLOCKED") {
        bail!("generic review verdict 非 PASS|FAIL|BLOCKED");
    }
    let body_len = u64::try_from(body.trim().len()).context("generic review body 长度溢出")?;
    if body_len == 0 {
        bail!("generic review 缺 substantive body");
    }
    Ok(CheckedGenericArtifact {
        verdict: parsed.verdict,
        body_len,
    })
}

#[cfg(unix)]
fn ensure_single_link(path: &Path, label: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat {label} 失败: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
        bail!("{label} 必须是 regular single-link no-symlink file");
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_single_link(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat {label} 失败: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{label} 必须是 regular no-symlink file");
    }
    Ok(())
}

fn ensure_safe_parent(root: &Path, path: &Path) -> Result<()> {
    let parent = path.parent().context("generic review path 缺 parent")?;
    let relative = parent
        .strip_prefix(root)
        .context("generic review parent 逃出 repository root")?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            bail!("generic review parent 含非 canonical component");
        };
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!(
                    "generic review parent 必须是 real directory: {}",
                    cursor.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cursor)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn require_existing_real_parent_chain(root: &Path, path: &Path, label: &str) -> Result<()> {
    let parent = path.parent().with_context(|| format!("{label} 缺 parent"))?;
    let relative = parent
        .strip_prefix(root)
        .with_context(|| format!("{label} parent 逃出 repository root"))?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            bail!("{label} parent 含非 canonical component");
        };
        cursor.push(component);
        let metadata = fs::symlink_metadata(&cursor)
            .with_context(|| format!("{label} parent 不存在: {}", cursor.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("{label} parent 必须是 existing real directory");
        }
    }
    Ok(())
}

fn install_new_canonical(root: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_safe_parent(root, path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create-new generic review 失败: {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    ensure_single_link(path, "generic review")
}

fn exact_request<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
) -> Result<&'a orch_core::EventRecord> {
    let requests = events
        .iter()
        .filter(|event| {
            event.kind == "ReviewRequested"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "attemptId") == Some(identity.attempt.as_str())
                && payload_string(event, "role") == Some(GENERIC_REVIEW_ROLE)
                && payload_string(event, "agent") == Some(identity.harness.as_str())
                && payload_string(event, "harness") == Some(identity.harness.as_str())
                && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    let [request] = requests.as_slice() else {
        bail!(
            "generic review 要求唯一 exact ReviewRequested，实得 {}",
            requests.len()
        );
    };
    Ok(request)
}

fn exact_wake<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
) -> Result<&'a orch_core::EventRecord> {
    let wakes = events
        .iter()
        .filter(|event| {
            event.kind == "WakeIssued"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "attemptId") == Some(identity.attempt.as_str())
                && payload_string(event, "agent") == Some(identity.harness.as_str())
                && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    let [wake] = wakes.as_slice() else {
        bail!(
            "generic review 要求唯一 exact WakeIssued，实得 {}",
            wakes.len()
        );
    };
    Ok(wake)
}

fn exact_terminal<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
) -> Result<Option<&'a orch_core::EventRecord>> {
    let terminals = events
        .iter()
        .filter(|event| {
            event.kind == "ManagedWakeTerminated"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "agent") == Some(identity.harness.as_str())
                && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    match terminals.as_slice() {
        [] => Ok(None),
        [terminal] => Ok(Some(terminal)),
        _ => bail!("generic review managed terminal 重复"),
    }
}

fn exact_backend_receipt<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
) -> Result<Option<&'a orch_core::EventRecord>> {
    let receipts = events
        .iter()
        .filter(|event| {
            event.kind == "AgentEventReceived"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "agentEvent") == Some("wake-backend-receipt")
                && (payload_string(event, "actionId") == Some(identity.wake_id.as_str())
                    || payload_string(event, "wakeId") == Some(identity.wake_id.as_str()))
        })
        .collect::<Vec<_>>();
    let wake = exact_wake(events, round, identity)?;
    for receipt in &receipts {
        if !crate::wake::accepted_backend_receipt_matches_wake(wake, receipt)? {
            bail!("bootstrap backend receipt 存在 anchored conflicting fact");
        }
    }
    match receipts.as_slice() {
        [] => Ok(None),
        [receipt] => Ok(Some(receipt)),
        _ => bail!("bootstrap backend receipt 重复"),
    }
}

// This archive reader trusts the runtime-minted terminal; cancel authentication
// occurs before ledger append in the managed supervisor/status path. A canonical
// cancel ID is a shape check, not an independent archived cancellation receipt.
// The shared validator still checks channel binding, order and exact release.
fn receiptless_terminal_is_closed_zero_vote(
    terminal: Option<&orch_core::EventRecord>,
    deliveries: &[&orch_core::EventRecord],
) -> bool {
    let Some(terminal) = terminal.filter(|_| deliveries.is_empty()) else {
        return false;
    };
    let payload = terminal.payload.as_ref();
    let nonanswer_exit = match (
        payload_string(terminal, "state"),
        payload_string(terminal, "outcomeClass"),
        payload_string(terminal, "completionReason"),
    ) {
        (Some("failed"), Some("TruncatedNoTerminal"), Some("natural-exit")) => true,
        (Some("timedOut"), Some("StoppedByHardDeadline"), Some("hard-deadline")) => {
            payload
                .and_then(|value| value.get("exitedNaturally"))
                .and_then(serde_json::Value::as_bool)
                == Some(false)
                && payload.and_then(|value| value.get("cancelRequestId"))
                    == Some(&serde_json::Value::Null)
        }
        (Some("canceled"), Some("StoppedByAuthenticatedCancel"), Some("manual-cancel")) => {
            payload
                .and_then(|value| value.get("exitedNaturally"))
                .and_then(serde_json::Value::as_bool)
                == Some(false)
                && payload
                    .and_then(|value| value.get("hardDeadlineReached"))
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
                && payload_string(terminal, "cancelRequestId")
                    .is_some_and(crate::wake::is_strict_uuid)
        }
        _ => false,
    };
    generic_review_terminal_closed_v1(terminal)
        && nonanswer_exit
        && payload_string(terminal, "exactReason")
            == Some("backend acceptance receipt absent before terminal reconciliation")
        && payload
            .and_then(|value| value.get("terminalSeen"))
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        && payload
            .and_then(|value| value.get("turnEnded"))
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        && payload
            .and_then(|value| value.get("mechanicalTerminalAbsent"))
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        && ["finalTextSha256", "outputPath", "outputSha256"]
            .into_iter()
            .all(|key| {
                payload
                    .and_then(|value| value.get(key))
                    .is_some_and(serde_json::Value::is_null)
            })
}

fn terminal_answered_shape(event: &orch_core::EventRecord, review_output_required: bool) -> bool {
    let payload = event.payload.as_ref();
    payload_string(event, "state") == Some("answered")
        && matches!(
            payload_string(event, "completionReason"),
            Some("natural-exit" | "exact-terminal")
        )
        && payload
            .and_then(|value| value.get("terminalSeen"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && payload
            .and_then(|value| value.get("turnEnded"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && payload
            .and_then(|value| value.get("managedScopeTerminated"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && if review_output_required {
            payload_string(event, "outputPath").is_some_and(|value| !value.is_empty())
                && payload_string(event, "outputSha256")
                    .is_some_and(|value| canonical_sha(value, 64))
        } else {
            payload_string(event, "finalTextSha256")
                .is_some_and(|value| canonical_sha(value, 64))
        }
}

fn terminal_answered(event: &orch_core::EventRecord) -> bool {
    terminal_answered_shape(event, true)
}

/// Validate the provider-neutral managed-terminal state/outcome matrix shared
/// by generic review truth projection and scheduler capacity release.
pub(crate) fn generic_review_terminal_closed_v1(event: &orch_core::EventRecord) -> bool {
    terminal_closed_v1(event, true)
}

/// Validate one unified-channel terminal with action-specific output truth.
pub(crate) fn unified_channel_terminal_closed_v1(
    event: &orch_core::EventRecord,
    review_action: bool,
) -> bool {
    terminal_closed_v1(event, review_action)
}

fn terminal_closed_v1(event: &orch_core::EventRecord, review_output_required: bool) -> bool {
    let Some(payload) = event.payload.as_ref() else {
        return false;
    };
    let Some(state) = payload_string(event, "state")
        .and_then(|value| crate::harness::TerminalState::parse(value).ok())
    else {
        return false;
    };
    let required_bool = |key: &str| payload.get(key).and_then(serde_json::Value::as_bool);
    let (
        Some(terminal_seen),
        Some(turn_ended),
        Some(exited_naturally),
        Some(hard_deadline),
        Some(managed),
    ) = (
        required_bool("terminalSeen"),
        required_bool("turnEnded"),
        required_bool("exitedNaturally"),
        required_bool("hardDeadlineReached"),
        required_bool("managedScopeTerminated"),
    )
    else {
        return false;
    };
    let cancel_request_id = match payload.get("cancelRequestId") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
        _ => return false,
    };
    let Some(signals) = payload
        .get("signals")
        .and_then(serde_json::Value::as_array)
        .and_then(|values| {
            values
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
    else {
        return false;
    };
    let Some(log_bytes_read) = payload
        .get("logBytesRead")
        .and_then(serde_json::Value::as_u64)
    else {
        return false;
    };
    let facts = crate::wake::ManagedWakeTerminationFacts {
        wake_id: payload_string(event, "wakeId")
            .unwrap_or_default()
            .to_string(),
        agent: payload_string(event, "agent")
            .unwrap_or_default()
            .to_string(),
        completion_reason: payload_string(event, "completionReason").map(str::to_string),
        terminal_seen,
        exited_naturally,
        hard_deadline_reached: hard_deadline,
        cancel_request_id,
        signals,
        managed_scope_terminated: managed,
        log_bytes_read,
    };
    let outcome = crate::wake::classify_managed_wake_outcome(&facts);
    let outcome_name = match outcome {
        crate::wake::ManagedWakeOutcomeClass::DeliveredTerminal => "DeliveredTerminal",
        crate::wake::ManagedWakeOutcomeClass::TruncatedNoTerminal => "TruncatedNoTerminal",
        crate::wake::ManagedWakeOutcomeClass::StoppedByHardDeadline => "StoppedByHardDeadline",
        crate::wake::ManagedWakeOutcomeClass::StoppedByAuthenticatedCancel => {
            "StoppedByAuthenticatedCancel"
        }
        crate::wake::ManagedWakeOutcomeClass::OperationalError(_) => "OperationalError",
    };
    if payload_string(event, "outcomeClass") != Some(outcome_name) || !managed {
        return false;
    }
    let state_shape = match state {
        crate::harness::TerminalState::Answered => {
            outcome_name == "DeliveredTerminal"
                && terminal_answered_shape(event, review_output_required)
        }
        crate::harness::TerminalState::Empty => {
            (outcome_name == "DeliveredTerminal" && terminal_seen && turn_ended)
                || (outcome_name == "TruncatedNoTerminal" && !terminal_seen && !turn_ended)
        }
        crate::harness::TerminalState::TimedOut => outcome_name == "StoppedByHardDeadline",
        crate::harness::TerminalState::Canceled => outcome_name == "StoppedByAuthenticatedCancel",
        crate::harness::TerminalState::Failed => {
            matches!(outcome_name, "OperationalError" | "TruncatedNoTerminal")
        }
    };
    state_shape
        && !(terminal_seen && !turn_ended)
        && payload_string(event, "completionReason").is_some_and(|value| !value.trim().is_empty())
        && payload_string(event, "exactReason").is_some_and(|value| !value.trim().is_empty())
}

// One-time r83/B320-A0001 archived decoder authorized by planning/062 §5.
// Exact incident pins plus a later canonical root/TaskRecorded keep this out
// of every live admission and pre-verdict path.
const B320_A0001_SMART_WAKE_ID: &str = "01a05d73-67af-4d4d-bfb8-e9820cc5c59b";
const B320_A0001_SMART_LEASE_EVENT_ID: &str = "01M1EQ6YVJ2R4QYQWFPTY1DHB5";
const B320_A0001_SMART_WAKE_EVENT_ID: &str = "01M1EQ6ZKDXDRBHBVD8TA978G7";
const B320_A0001_SMART_REQUEST_EVENT_ID: &str = "01M1EQ6ZKDXHAP6PW1P1YPFV94";
const B320_A0001_SMART_RECEIPT_EVENT_ID: &str = "01M1ESQCGDSWJDCRFAMH09FGER";
const B320_A0001_SMART_TERMINAL_EVENT_ID: &str = "01M1ESQCPY4AA81HXJ4636Z8RV";
const B320_A0001_SMART_RELEASE_EVENT_ID: &str = "01M1ESQCPYGX62EJEXP7C66HWH";

fn bootstrap_projection_mismatch_closed_for_nonpass(
    events: &[orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
    request: &orch_core::EventRecord,
    wake: &orch_core::EventRecord,
    lease: &orch_core::EventRecord,
    terminal: &orch_core::EventRecord,
    receipt: Option<&orch_core::EventRecord>,
    verdict: crate::verify::RootVerdict,
) -> Result<bool> {
    if !matches!(
        verdict,
        crate::verify::RootVerdict::Fail | crate::verify::RootVerdict::Blocked
    ) || round != "r83"
        || identity.task != "B320"
        || identity.attempt != "B320-A0001"
        || identity.harness != "smartclaw"
        || identity.wake_id != B320_A0001_SMART_WAKE_ID
        || lease.event_id != B320_A0001_SMART_LEASE_EVENT_ID
        || wake.event_id != B320_A0001_SMART_WAKE_EVENT_ID
        || request.event_id != B320_A0001_SMART_REQUEST_EVENT_ID
        || terminal.event_id != B320_A0001_SMART_TERMINAL_EVENT_ID
    {
        return Ok(false);
    }
    let archived_root = events.iter().any(|event| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(identity.task.as_str())
            && payload_string(event, "attemptId") == Some(identity.attempt.as_str())
    });
    let task_recorded = events.iter().any(|event| {
        event.kind == "TaskRecorded"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(identity.task.as_str())
            && payload_string(event, "postMergeGates") == Some("all-green")
    });
    if !archived_root || !task_recorded {
        return Ok(false);
    }
    let Some(receipt) = receipt else {
        return Ok(false);
    };
    if receipt.event_id != B320_A0001_SMART_RECEIPT_EVENT_ID {
        return Ok(false);
    }
    let Some(terminal_payload) = terminal.payload.as_ref() else {
        return Ok(false);
    };
    let Some(receipt_payload) = receipt.payload.as_ref() else {
        return Ok(false);
    };
    let Some(wake_payload) = wake.payload.as_ref() else {
        return Ok(false);
    };
    let Some(request_payload) = request.payload.as_ref() else {
        return Ok(false);
    };
    let Some(lease_payload) = lease.payload.as_ref() else {
        return Ok(false);
    };
    let binding_keys = [
        "attachmentManifestSha256",
        "commandDigest",
        "configDigest",
        "cwdSelection",
        "driver",
        "effectiveTuple",
        "executableIdentityDigest",
        "fixedHead",
        "harness",
        "invocationCwd",
        "observationSource",
        "requestDigest",
        "requestedTuple",
    ];
    let mut expected_binding = serde_json::Map::new();
    for key in binding_keys {
        let Some(value) = wake_payload.get(key) else {
            return Ok(false);
        };
        if request_payload.get(key) != Some(value) {
            return Ok(false);
        }
        expected_binding.insert(key.to_string(), value.clone());
    }
    let expected_binding = serde_json::Value::Object(expected_binding);
    if receipt_payload.get("channelBinding") != Some(&expected_binding)
        || terminal_payload.get("channelBinding") != Some(&expected_binding)
    {
        return Ok(false);
    }
    let releases = events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceReleased"
                && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    let [release] = releases.as_slice() else {
        return Ok(false);
    };
    if release.event_id != B320_A0001_SMART_RELEASE_EVENT_ID {
        return Ok(false);
    }
    let Some(release_payload) = release.payload.as_ref() else {
        return Ok(false);
    };
    let lease_position = unique_position(events, &lease.event_id)?;
    let wake_position = unique_position(events, &wake.event_id)?;
    let request_position = unique_position(events, &request.event_id)?;
    let receipt_position = unique_position(events, &receipt.event_id)?;
    let terminal_position = unique_position(events, &terminal.event_id)?;
    let release_position = unique_position(events, &release.event_id)?;
    let ordered = lease_position < wake_position
        && wake_position < request_position
        && request_position < receipt_position
        && receipt_position < terminal_position
        && terminal_position + 1 == release_position;
    let release_tuple_matches = ["attemptId", "role", "agent", "wakeId"]
        .iter()
        .all(|key| lease_payload.get(*key) == release_payload.get(*key));
    let expected_session = format!("orch-wake-{}", identity.wake_id);
    let output_path_matches = payload_string(request, "reviewOutputPath")
        .is_some_and(|expected| payload_string(terminal, "outputPath") == Some(expected));
    let valid = ordered
        && payload_string(wake, "action") == Some("review")
        && payload_string(wake, "method") == Some("unified-channel-v1")
        && payload_string(wake, "driver") == Some("smartclaw")
        && payload_string(wake, "providerKind") == Some("smartclaw")
        && payload_string(wake, "requestSessionId") == Some(expected_session.as_str())
        && payload_string(wake, "harness") == Some(identity.harness.as_str())
        && lease.actor == "runtime:orch"
        && payload_string(lease, "siteId") == Some("B320-review-smartclaw-g01")
        && lease_payload
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
        && payload_string(receipt, "requestSessionId") == Some(expected_session.as_str())
        && payload_string(receipt, "observedSessionId") == Some(expected_session.as_str())
        && release.actor == "runtime:orch"
        && release.round.as_deref() == Some(round)
        && release.task_id.as_deref() == Some(identity.task.as_str())
        && release_tuple_matches
        && payload_string(release, "siteId") == Some("B320-review-smartclaw-g01")
        && release_payload
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            == Some(1)
        && payload_string(release, "completionReceipt")
            == Some("runtime:orch/managed-wake-terminated")
        && payload_string(release, "terminationEventId") == Some(terminal.event_id.as_str())
        && payload_string(terminal, "state") == Some("answered")
        && payload_string(terminal, "completionReason") == Some("natural-exit")
        && payload_string(terminal, "outcomeClass") == Some("TruncatedNoTerminal")
        && terminal_payload
            .get("terminalSeen")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        && terminal_payload
            .get("turnEnded")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && terminal_payload
            .get("exitedNaturally")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && terminal_payload
            .get("hardDeadlineReached")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        && terminal_payload
            .get("managedScopeTerminated")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        && terminal_payload
            .get("mechanicalTerminalAbsent")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        && terminal_payload
            .get("cancelRequestId")
            .is_some_and(serde_json::Value::is_null)
        && terminal_payload
            .get("signals")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty)
        && receipt_payload
            .get("probeEnd")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|probe_end| {
                probe_end > 0
                    && terminal_payload
                        .get("logBytesRead")
                        .and_then(serde_json::Value::as_u64)
                        == Some(probe_end)
            })
        && payload_string(terminal, "exactReason").is_some_and(|value| !value.trim().is_empty())
        && output_path_matches
        && payload_string(terminal, "outputSha256").is_some_and(|value| canonical_sha(value, 64))
        && payload_string(terminal, "finalTextSha256")
            .is_some_and(|value| canonical_sha(value, 64));
    Ok(valid)
}

fn unique_position(events: &[orch_core::EventRecord], event_id: &str) -> Result<usize> {
    let positions = events
        .iter()
        .enumerate()
        .filter_map(|(position, event)| (event.event_id == event_id).then_some(position))
        .collect::<Vec<_>>();
    match positions.as_slice() {
        [position] => Ok(*position),
        _ => bail!("generic review eventId 非全局唯一: {event_id}"),
    }
}

/// Distinguish adjudication quarantine from launch or acceptance-receipt failure.
pub(crate) const REVIEW_QUARANTINE_OPERATION: &str = "review-quarantine";

/// Recognize the operation tag only; this does not validate or authorize the record.
pub(crate) fn is_review_quarantine(event: &orch_core::EventRecord) -> bool {
    event.kind == "ActionRejected"
        && payload_string(event, "operation") == Some(REVIEW_QUARANTINE_OPERATION)
}

/// Check the closed rejection envelope without inferring process or native completion.
pub(crate) fn validate_review_quarantine_shape(event: &orch_core::EventRecord) -> Result<()> {
    let payload = event.payload.as_ref().and_then(serde_json::Value::as_object)
        .context("review quarantine payload must be an object")?;
    if !is_review_quarantine(event)
        || event.actor != "runtime:orch"
        || event.round.as_deref().is_none_or(str::is_empty)
        || event.task_id.as_deref().is_none_or(str::is_empty)
        || payload.len() != 7
        || payload_string(event, "actionId").is_none_or(str::is_empty)
        || payload_string(event, "attemptId").is_none_or(str::is_empty)
        || payload.get("attemptNo").and_then(serde_json::Value::as_u64).is_none_or(|n| n == 0)
        || payload.get("exitCode").and_then(serde_json::Value::as_i64) != Some(5)
        || payload.get("alert").and_then(serde_json::Value::as_bool) != Some(true)
        || payload_string(event, "reason").is_none_or(|s| s.is_empty() || s.trim() != s)
    {
        bail!("review quarantine rejection envelope is not exact");
    }
    Ok(())
}

/// Recognize only a complete quarantine block immediately paired with root BLOCKED.
/// The accepted request remains physically pending; no cleanup fact is returned.
pub(crate) fn canonical_review_quarantine(
    event: &orch_core::EventRecord,
    events: &[orch_core::EventRecord],
    round: &str,
) -> Result<bool> {
    if !is_review_quarantine(event) { return Ok(false); }
    validate_review_quarantine_shape(event)?;
    if event.round.as_deref() != Some(round) { bail!("review quarantine round mismatch"); }
    if crate::round::contract_schema_from_events(events, round)?
        != Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    { bail!("review quarantine requires schema3 history"); }
    let position = unique_position(events, &event.event_id)?;
    let root_position = events.iter().enumerate().skip(position + 1)
        .find(|(_, candidate)| !is_review_quarantine(candidate))
        .map(|(position, _)| position)
        .context("review quarantine has no adjacent root BLOCKED partner")?;
    let root = &events[root_position];
    if root.kind != "VerdictIssued" || root.actor != "verifier:root"
        || root.round != event.round || root.task_id != event.task_id
    { bail!("review quarantine is not adjacent to its root BLOCKED partner"); }
    let root_payload: crate::verify::RootVerdictPayload = serde_json::from_value(
        root.payload.clone().context("review quarantine root payload missing")?
    ).context("review quarantine root payload is not canonical")?;
    if root_payload.verdict != "BLOCKED"
        || Some(root_payload.attempt_id.as_str()) != payload_string(event, "attemptId")
        || root_payload.attempt_no as u64 != event.payload.as_ref().unwrap()["attemptNo"].as_u64().unwrap()
        || root_payload.reason.as_deref() != payload_string(event, "reason")
        || !canonical_sha(&root_payload.head_sha, 40)
        || !canonical_sha(&root_payload.main_head_sha, 40)
    { bail!("review quarantine root identity/reason/verdict mismatch"); }
    let task = event.task_id.as_deref().unwrap();
    let wake_id = payload_string(event, "actionId").unwrap();
    let prefix = &events[..position];
    let requests = prefix.iter().filter(|candidate| candidate.kind == "ReviewRequested"
        && payload_string(candidate, "wakeId") == Some(wake_id)).collect::<Vec<_>>();
    let [request] = requests.as_slice() else { bail!("review quarantine needs one preceding request"); };
    let identity = request_identity(request)?;
    if identity.task != task || identity.attempt != root_payload.attempt_id
        || identity.harness == "local" || identity.wake_id != wake_id
    { bail!("review quarantine request identity mismatch"); }
    let request = exact_request(prefix, round, &identity)?;
    if prefix.iter().filter(|candidate| candidate.kind == "WakeIssued"
        && payload_string(candidate, "wakeId") == Some(wake_id)).count() != 1
        || prefix.iter().filter(|candidate| candidate.kind == "WorkspaceLeased"
            && payload_string(candidate, "wakeId") == Some(wake_id)).count() != 1
        || prefix.iter().filter(|candidate| candidate.kind == "AgentEventReceived"
            && payload_string(candidate, "agentEvent") == Some("wake-backend-receipt")
            && (payload_string(candidate, "wakeId") == Some(wake_id)
                || payload_string(candidate, "actionId") == Some(wake_id))).count() != 1
    { bail!("review quarantine has missing, duplicate or foreign request lineage"); }
    let wake = exact_wake(prefix, round, &identity)?;
    if payload_string(wake, "method") != Some("unified-channel-v1")
        || payload_string(wake, "action") != Some("review")
        || payload_string(wake, "controlWakeId") != Some(wake_id)
        || payload_string(wake, "backendState") != Some("pending")
        || payload_string(wake, "fixedHead") != Some(root_payload.head_sha.as_str())
    { bail!("review quarantine requires a pending unified review wake"); }
    let request_position = unique_position(events, &request.event_id)?;
    let wake_position = unique_position(events, &wake.event_id)?;
    let receipt = exact_backend_receipt(prefix, round, &identity)?
        .context("review quarantine requires a bound accepted backend receipt")?;
    let receipt_position = unique_position(events, &receipt.event_id)?;
    crate::wake::validate_existing_channel_action_binding(wake, receipt)?;
    if !(wake_position < request_position && request_position < receipt_position && receipt_position < position) {
        bail!("review quarantine request/receipt order is invalid");
    }
    let collects = events[..wake_position].iter().filter(|candidate| candidate.kind == "ReportCollectCompleted"
        && candidate.actor == "runtime:orch" && candidate.round.as_deref() == Some(round)
        && candidate.task_id.as_deref() == Some(task)
        && payload_string(candidate, "attemptId") == Some(identity.attempt.as_str())
        && payload_string(candidate, "branchSha") == Some(root_payload.head_sha.as_str())).count();
    if collects != 1 { bail!("review quarantine requires one preceding fixed-head collect"); }
    let leases = events[..wake_position].iter().filter(|candidate| candidate.kind == "WorkspaceLeased"
        && candidate.actor == "runtime:orch" && candidate.round.as_deref() == Some(round)
        && candidate.task_id.as_deref() == Some(task)
        && payload_string(candidate, "attemptId") == Some(identity.attempt.as_str())
        && payload_string(candidate, "role") == Some("review")
        && payload_string(candidate, "agent") == Some(identity.harness.as_str())
        && payload_string(candidate, "wakeId") == Some(wake_id)
        && payload_string(candidate, "reviewedHead") == Some(root_payload.head_sha.as_str())).collect::<Vec<_>>();
    let [lease] = leases.as_slice() else { bail!("review quarantine needs one exact preceding lease"); };
    let worktree = lease.payload.as_ref().and_then(|p| p.get("paths"))
        .and_then(|p| p.get("worktree")).and_then(serde_json::Value::as_str)
        .context("review quarantine lease worktree missing")?;
    let relative = Path::new(worktree);
    if relative.as_os_str().is_empty()
        || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        bail!("review quarantine lease worktree is not canonical relative");
    }
    validate_channel_request_facts(request, wake, &identity, round, &root_payload.head_sha, relative)?;
    for candidate in &events[..root_position] {
        let same_wake = payload_string(candidate, "wakeId") == Some(wake_id)
            || payload_string(candidate, "actionId") == Some(wake_id);
        if same_wake && (matches!(candidate.kind.as_str(), "ManagedWakeTerminated" | "WorkspaceReleased" | "ReviewDelivered")
            || (candidate.kind == "ActionRejected" && candidate.event_id != event.event_id))
        { bail!("review quarantine cannot replace a terminal/delivery/rejection"); }
    }
    if events[..root_position].iter().any(|candidate| candidate.kind == "VerdictIssued"
        && candidate.actor == "verifier:root" && candidate.round.as_deref() == Some(round)
        && candidate.task_id.as_deref() == Some(task)
        && payload_string(candidate, "attemptId") == Some(identity.attempt.as_str()))
    { bail!("review quarantine cannot follow an earlier root verdict"); }
    Ok(true)
}

/// Prepare new quarantines or recover the exact previously recorded selection.
/// Only the caller's checked root BLOCKED batch may publish these inert review refusals.
pub(crate) fn prepare_review_quarantines(
    events: &[orch_core::EventRecord], round: &str, task: &str, attempt: &str,
    reason: &str, selected: &[String],
) -> Result<Vec<orch_core::EventRecord>> {
    let wanted = selected.iter().cloned().collect::<BTreeSet<_>>();
    if wanted.len() != selected.len() { bail!("duplicate --quarantine-review wake id"); }
    let existing = events.iter().filter(|event| is_review_quarantine(event)
        && event.round.as_deref() == Some(round) && event.task_id.as_deref() == Some(task)
        && payload_string(event, "attemptId") == Some(attempt)).collect::<Vec<_>>();
    let roots = events.iter().filter(|event| event.kind == "VerdictIssued" && event.actor == "verifier:root"
        && event.round.as_deref() == Some(round) && event.task_id.as_deref() == Some(task)
        && payload_string(event, "attemptId") == Some(attempt)).count();
    if !existing.is_empty() || roots > 0 {
        if roots != 1 { bail!("recorded quarantine lacks unique root verdict"); }
        let observed = existing.iter().map(|event| {
            canonical_review_quarantine(event, events, round)?;
            if payload_string(event, "reason") != Some(reason) { bail!("review quarantine replay reason changed"); }
            Ok(payload_string(event, "actionId").unwrap().to_string())
        }).collect::<Result<BTreeSet<_>>>()?;
        if observed != wanted || observed.len() != existing.len() {
            bail!("review quarantine replay selection changed");
        }
        return Ok(Vec::new());
    }
    let current = crate::attempt::current_attempt(events, task)?.context("review quarantine lacks current attempt")?;
    if current.attempt_id != attempt { bail!("review quarantine attempt is not current"); }
    wanted.into_iter().map(|wake_id| {
        if wake_id.is_empty() || !wake_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            bail!("review quarantine wake id is not canonical");
        }
        if events.iter().any(|event| event.kind == "ActionRejected" && payload_string(event, "actionId") == Some(wake_id.as_str())) {
            bail!("review quarantine cannot replace an existing rejection");
        }
        let rejection = crate::failure::ActionRejection::from_disposition(REVIEW_QUARANTINE_OPERATION,
            &wake_id, reason, crate::failure::CliDisposition::EffectUnknown)?.with_attempt(&current);
        Ok(crate::failure::rejection_event(round, Some(task), &rejection))
    }).collect()
}

fn exact_rejection<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
) -> Result<Option<&'a orch_core::EventRecord>> {
    let matching = events
        .iter()
        .filter(|event| {
            event.kind == "ActionRejected"
                && payload_string(event, "actionId") == Some(identity.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => Ok(None),
        [event] => {
            let payload = event
                .payload
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .context("generic ActionRejected payload 非 object")?;
            if event.actor != "runtime:orch"
                || event.round.as_deref() != Some(round)
                || event.task_id.as_deref() != Some(identity.task.as_str())
                || payload.len() != 7
                || !matches!(payload_string(event, "operation"), Some("wake-backend-receipt" | "review-quarantine"))
                || payload_string(event, "attemptId") != Some(identity.attempt.as_str())
                || payload
                    .get("attemptNo")
                    .and_then(serde_json::Value::as_u64)
                    .is_none_or(|value| value == 0)
                || payload
                    .get("exitCode")
                    .and_then(serde_json::Value::as_i64)
                    .is_none_or(|value| !(1..=255).contains(&value))
                || payload.get("alert").and_then(serde_json::Value::as_bool) != Some(true)
                || payload_string(event, "reason").is_none_or(|value| value.trim().is_empty())
            {
                bail!("generic ActionRejected 非 exact typed rejection");
            }
            if is_review_quarantine(event) { validate_review_quarantine_shape(event)?; }
            Ok(Some(*event))
        }
        _ => bail!("generic review ActionRejected 重复"),
    }
}

fn exact_release<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    identity: &GenericReviewIdentity,
    lease: &orch_core::EventRecord,
    terminal: &orch_core::EventRecord,
) -> Result<&'a orch_core::EventRecord> {
    let matching = events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceReleased"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    let [release] = matching.as_slice() else {
        bail!(
            "generic review 要求唯一 exact WorkspaceReleased，实得 {}",
            matching.len()
        );
    };
    let lease_payload = lease
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("generic WorkspaceLeased payload 非 object")?;
    let release_payload = release
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("generic WorkspaceReleased payload 非 object")?;
    for key in [
        "siteId",
        "generation",
        "attemptId",
        "role",
        "agent",
        "wakeId",
    ] {
        if lease_payload.get(key) != release_payload.get(key) {
            bail!("generic WorkspaceReleased 与 lease {key} 漂移");
        }
    }
    if release.actor != "runtime:orch"
        || payload_string(release, "completionReceipt")
            != Some("runtime:orch/managed-wake-terminated")
        || payload_string(release, "terminationEventId") != Some(terminal.event_id.as_str())
    {
        bail!("generic WorkspaceReleased completion binding 漂移");
    }
    let terminal_position = unique_position(events, &terminal.event_id)?;
    let release_position = unique_position(events, &release.event_id)?;
    if terminal_position + 1 != release_position {
        bail!("generic WorkspaceReleased 必须紧邻 exact terminal");
    }
    Ok(release)
}

/// Require the exact unified request, terminal binding, lease, and adjacent
/// release used by both review validation and scheduler capacity projection.
pub(crate) fn generic_review_terminal_release_closed_v1(
    events: &[orch_core::EventRecord],
    round: &str,
    task: &str,
    attempt: &str,
    harness: &str,
    wake_id: &str,
) -> Result<()> {
    let identity = GenericReviewIdentity::new(task, attempt, harness, wake_id)?;
    let request = exact_request(events, round, &identity)?;
    let wake = exact_wake(events, round, &identity)?;
    if payload_string(wake, "method") != Some("unified-channel-v1")
        || payload_string(wake, "action") != Some("review")
    {
        bail!("generic terminal/release closure 只接受 unified review");
    }
    let terminal = exact_terminal(events, round, &identity)?
        .context("generic terminal/release closure 缺 terminal")?;
    crate::wake::validate_existing_channel_action_binding(wake, terminal)?;
    if !generic_review_terminal_closed_v1(terminal) {
        bail!("generic terminal/release closure terminal 非可信矩阵");
    }
    let leases = events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceLeased"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
                && payload_string(event, "role") == Some(GENERIC_REVIEW_ROLE)
                && payload_string(event, "agent") == Some(harness)
                && payload_string(event, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    let [lease] = leases.as_slice() else {
        bail!("generic terminal/release closure 缺唯一 lease");
    };
    let release = exact_release(events, round, &identity, lease, terminal)?;
    let lease_position = unique_position(events, &lease.event_id)?;
    let wake_position = unique_position(events, &wake.event_id)?;
    let request_position = unique_position(events, &request.event_id)?;
    let terminal_position = unique_position(events, &terminal.event_id)?;
    let release_position = unique_position(events, &release.event_id)?;
    if !(lease_position < wake_position
        && wake_position < request_position
        && request_position < terminal_position
        && terminal_position + 1 == release_position)
    {
        bail!("generic lease/wake/request/terminal/release 顺序漂移");
    }
    Ok(())
}

/// Recognize only the one recorded pre-terminal generic review from r83/B319
/// so legacy archive capacity does not leak into later live scheduling.
pub(crate) fn historical_b319_capacity_closed_v1(
    events: &[orch_core::EventRecord],
    round: &str,
    task: &str,
    attempt: &str,
    harness: &str,
    wake_id: &str,
) -> Result<bool> {
    if round != "r83" || task != "B319" || attempt != "B319-A0001" {
        return Ok(false);
    }
    let identity = GenericReviewIdentity::new(task, attempt, harness, wake_id)?;
    let request = exact_request(events, round, &identity)?;
    let wake = exact_wake(events, round, &identity)?;
    if payload_string(wake, "method") == Some("unified-channel-v1")
        || wake
            .payload
            .as_ref()
            .is_none_or(|payload| payload.get("bootstrapSourceSha256").is_none())
    {
        return Ok(false);
    }
    let Some(receipt) = exact_backend_receipt(events, round, &identity)? else {
        return Ok(false);
    };
    let deliveries = events
        .iter()
        .filter(|event| {
            event.kind == "ReviewDelivered"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
                && payload_string(event, "role") == Some(GENERIC_REVIEW_ROLE)
                && payload_string(event, "agent") == Some(harness)
                && payload_string(event, "harness") == Some(harness)
                && payload_string(event, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    let roots = events
        .iter()
        .filter(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
        })
        .collect::<Vec<_>>();
    let recorded = events
        .iter()
        .filter(|event| {
            event.kind == "TaskRecorded"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "postMergeGates") == Some("all-green")
        })
        .collect::<Vec<_>>();
    let ([delivery], [root], [recorded]) =
        (deliveries.as_slice(), roots.as_slice(), recorded.as_slice())
    else {
        return Ok(false);
    };
    let fixed_head = payload_string(wake, "fixedHead").unwrap_or_default();
    let exact = canonical_sha(fixed_head, 40)
        && payload_string(delivery, "requestEventId") == Some(request.event_id.as_str())
        && payload_string(delivery, "terminalEventId") == Some(receipt.event_id.as_str())
        && payload_string(delivery, "reviewedHead") == Some(fixed_head)
        && delivery
            .payload
            .as_ref()
            .and_then(|payload| payload.get("bodyLen"))
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|bytes| bytes > 0)
        && payload_string(root, "verdict") == Some("PASS")
        && payload_string(root, "headSha") == Some(fixed_head);
    if !exact {
        return Ok(false);
    }
    let wake_position = unique_position(events, &wake.event_id)?;
    let request_position = unique_position(events, &request.event_id)?;
    let receipt_position = unique_position(events, &receipt.event_id)?;
    let delivery_position = unique_position(events, &delivery.event_id)?;
    let root_position = unique_position(events, &root.event_id)?;
    let recorded_position = unique_position(events, &recorded.event_id)?;
    Ok(wake_position < request_position
        && request_position < receipt_position
        && receipt_position < delivery_position
        && delivery_position < root_position
        && root_position < recorded_position)
}

/// Close only the pinned B320-A0001 SmartClaw projection mismatch after its
/// canonical BLOCKED root and later all-green task record are both durable.
pub(crate) fn historical_b320_capacity_closed_v1(
    events: &[orch_core::EventRecord],
    round: &str,
    task: &str,
    attempt: &str,
    harness: &str,
    wake_id: &str,
) -> Result<bool> {
    if round != "r83"
        || task != "B320"
        || attempt != "B320-A0001"
        || harness != "smartclaw"
        || wake_id != B320_A0001_SMART_WAKE_ID
    {
        return Ok(false);
    }
    let identity = GenericReviewIdentity::new(task, attempt, harness, wake_id)?;
    let find = |event_id: &str| {
        events
            .iter()
            .find(|event| event.event_id == event_id)
            .with_context(|| format!("historical B320 capacity closure 缺 {event_id}"))
    };
    let lease = find(B320_A0001_SMART_LEASE_EVENT_ID)?;
    let wake = find(B320_A0001_SMART_WAKE_EVENT_ID)?;
    let request = find(B320_A0001_SMART_REQUEST_EVENT_ID)?;
    let receipt = find(B320_A0001_SMART_RECEIPT_EVENT_ID)?;
    let terminal = find(B320_A0001_SMART_TERMINAL_EVENT_ID)?;
    if !bootstrap_projection_mismatch_closed_for_nonpass(
        events,
        round,
        &identity,
        request,
        wake,
        lease,
        terminal,
        Some(receipt),
        crate::verify::RootVerdict::Blocked,
    )? {
        return Ok(false);
    }
    let roots = events
        .iter()
        .filter(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
                && payload_string(event, "verdict") == Some("BLOCKED")
                && payload_string(event, "headSha") == payload_string(wake, "fixedHead")
        })
        .collect::<Vec<_>>();
    let recorded = events
        .iter()
        .filter(|event| {
            event.kind == "TaskRecorded"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "postMergeGates") == Some("all-green")
        })
        .collect::<Vec<_>>();
    let ([root], [recorded]) = (roots.as_slice(), recorded.as_slice()) else {
        return Ok(false);
    };
    Ok(unique_position(events, &root.event_id)? < unique_position(events, &recorded.event_id)?)
}

fn request_identity(event: &orch_core::EventRecord) -> Result<GenericReviewIdentity> {
    let required = |key: &str| {
        payload_string(event, key)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("generic ReviewRequested 缺 {key}"))
    };
    let task = event
        .task_id
        .clone()
        .filter(|value| !value.is_empty())
        .context("generic ReviewRequested 缺 taskId")?;
    let harness = required("harness")?.to_string();
    if required("role")? != GENERIC_REVIEW_ROLE || required("agent")? != harness {
        bail!("generic ReviewRequested role/agent/harness 非 canonical");
    }
    Ok(GenericReviewIdentity {
        task,
        attempt: required("attemptId")?.to_string(),
        harness,
        wake_id: required("wakeId")?.to_string(),
    })
}

fn validate_delivery_event(
    event: &orch_core::EventRecord,
    identity: &GenericReviewIdentity,
    round: &str,
    reviewed_head: &str,
    request: &orch_core::EventRecord,
    completion: &orch_core::EventRecord,
    canonical_rel: &str,
    bytes: &[u8],
) -> Result<()> {
    let payload = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("generic ReviewDelivered payload 非 object")?;
    let checked = check_artifact(bytes, identity, round, reviewed_head)?;
    let sha = hex::encode(Sha256::digest(bytes));
    if event.kind != "ReviewDelivered"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(identity.task.as_str())
        || payload.len() != 13
        || payload_string(event, "attemptId") != Some(identity.attempt.as_str())
        || payload_string(event, "role") != Some(GENERIC_REVIEW_ROLE)
        || payload_string(event, "agent") != Some(identity.harness.as_str())
        || payload_string(event, "harness") != Some(identity.harness.as_str())
        || payload_string(event, "wakeId") != Some(identity.wake_id.as_str())
        || payload_string(event, "reviewedHead") != Some(reviewed_head)
        || payload_string(event, "requestEventId") != Some(request.event_id.as_str())
        || payload_string(event, "terminalEventId") != Some(completion.event_id.as_str())
        || payload_string(event, "path") != Some(canonical_rel)
        || payload_string(event, "sha256") != Some(sha.as_str())
        || payload_string(event, "verdict") != Some(checked.verdict.as_str())
        || payload.get("bytes").and_then(serde_json::Value::as_u64) != Some(bytes.len() as u64)
        || payload.get("bodyLen").and_then(serde_json::Value::as_u64) != Some(checked.body_len)
    {
        bail!("generic ReviewDelivered exact provenance/bytes 漂移");
    }
    Ok(())
}

fn parse_committed_event_records(bytes: &[u8]) -> Result<Vec<orch_core::EventRecord>> {
    if bytes.is_empty() || !bytes.ends_with(b"\n") {
        bail!("generic review ledger 必须是 non-empty LF-terminated JSONL");
    }
    let text = std::str::from_utf8(bytes).context("committed generic review ledger 非 UTF-8")?;
    let events = text
        .strip_suffix('\n')
        .expect("LF termination checked")
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            if line.is_empty() || line.trim() != line {
                bail!("generic review ledger 第 {} 行含空白/空行", index + 1);
            }
            let event = serde_json::from_str::<orch_core::EventRecord>(line).with_context(|| {
                format!("generic review ledger 第 {} 行非 canonical JSON", index + 1)
            })?;
            if serde_json::to_vec(&event)? != line.as_bytes() {
                bail!("generic review ledger 第 {} 行非 canonical bytes", index + 1);
            }
            Ok(event)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut event_ids = BTreeSet::new();
    if events
        .iter()
        .any(|event| event.event_id.trim().is_empty() || !event_ids.insert(event.event_id.clone()))
    {
        bail!("generic review ledger 含空白或重复 eventId");
    }
    Ok(events)
}

fn same_event_record(left: &orch_core::EventRecord, right: &orch_core::EventRecord) -> bool {
    left.event_id == right.event_id
        && left.ts == right.ts
        && left.actor == right.actor
        && left.kind == right.kind
        && left.task_id == right.task_id
        && left.round == right.round
        && left.payload == right.payload
        && left.extra == right.extra
}

fn committed_regular_blob_bytes(
    root: &Path,
    treeish: &str,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    let literal = format!(":(literal){path}");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-tree",
            "--full-tree",
            "-z",
            treeish,
            "--",
            literal.as_str(),
        ])
        .output()
        .context("generic review committed artifact ls-tree 启动失败")?;
    if !output.status.success() {
        bail!(
            "generic review committed artifact ls-tree 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let entries = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    let [entry] = entries.as_slice() else {
        if entries.is_empty() {
            return Ok(None);
        }
        bail!("generic review committed artifact path 非唯一");
    };
    let entry = std::str::from_utf8(entry).context("generic artifact ls-tree 非 UTF-8")?;
    let (header, observed_path) = entry
        .split_once('\t')
        .context("generic artifact ls-tree 缺 tab")?;
    let fields = header.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3
        || !matches!(fields[0], "100644" | "100755")
        || fields[1] != "blob"
        || observed_path != path
    {
        bail!("generic review committed artifact 非 exact regular blob");
    }
    crate::gitx::show_bytes(root, treeish, path).map(Some)
}

#[derive(Debug)]
enum GenericDeliveryStorageState {
    Normal {
        main_sha: String,
        ledger_bytes: Vec<u8>,
    },
    Recoverable {
        delivery: orch_core::EventRecord,
        bytes: Vec<u8>,
        ledger_bytes: Vec<u8>,
        main_sha: String,
    },
    Committed {
        delivery: orch_core::EventRecord,
        bytes: Vec<u8>,
        ledger_bytes: Vec<u8>,
        main_sha: String,
    },
}

fn generic_delivery_storage_state(
    root: &Path,
    round: &str,
    identity: &GenericReviewIdentity,
    observed_events: &[orch_core::EventRecord],
) -> Result<GenericDeliveryStorageState> {
    let canonical_rel = crate::verify::canonical_review_artifact_relpath(
        round,
        &identity.attempt,
        GENERIC_REVIEW_ROLE,
        &identity.harness,
    );
    let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let working_ledger = fs::read(root.join(&ledger_rel))?;
    let working_events = parse_committed_event_records(&working_ledger)?;
    if working_events.len() != observed_events.len()
        || working_events
            .iter()
            .zip(observed_events)
            .any(|(left, right)| !same_event_record(left, right))
    {
        bail!("generic review working ledger snapshot 漂移");
    }
    let wal = root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl"));
    require_existing_real_parent_chain(root, &wal, "generic review WAL")?;
    ensure_single_link(&wal, "generic review WAL")?;
    if fs::read(&wal)? != working_ledger {
        bail!("generic review working ledger/WAL 漂移");
    }
    let main_sha = crate::gitx::rev_parse(root, "refs/heads/main^{commit}")?;
    let main_ledger = crate::gitx::show_bytes(root, &main_sha, &ledger_rel)?;
    let main_events = parse_committed_event_records(&main_ledger)?;
    if !working_ledger.starts_with(&main_ledger)
        || (working_ledger.len() > main_ledger.len() && !main_ledger.ends_with(b"\n"))
    {
        bail!("generic review working ledger 不是 captured main 的 LF 扩展");
    }
    let related = |event: &&orch_core::EventRecord| {
        event.kind == "ReviewDelivered"
            && (payload_string(event, "path") == Some(canonical_rel.as_str())
                || (event.task_id.as_deref() == Some(identity.task.as_str())
                    && payload_string(event, "attemptId") == Some(identity.attempt.as_str())
                    && payload_string(event, "harness") == Some(identity.harness.as_str())
                    && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())))
    };
    let working_delivery = working_events.iter().filter(related).collect::<Vec<_>>();
    let main_delivery = main_events.iter().filter(related).collect::<Vec<_>>();
    if working_delivery.len() > 1 || main_delivery.len() > 1 {
        bail!("generic review storage census delivery identity/path 重复");
    }
    let requested_delivery = working_delivery.first().copied();
    let foreign_delivery_in_suffix = working_events[main_events.len()..].iter().any(|event| {
        event.kind == "ReviewDelivered"
            && requested_delivery.is_none_or(|requested| !std::ptr::eq(event, requested))
    });
    let working_delivery = working_delivery.first().copied().cloned();
    let main_delivery = main_delivery.first().copied().cloned();
    let working_artifact = crate::verify::optional_review_artifact_bytes(
        root,
        &root.join(&canonical_rel),
        "generic review working canonical census",
    )?;
    let main_artifact = committed_regular_blob_bytes(root, &main_sha, &canonical_rel)?;
    match (
        working_delivery,
        working_artifact,
        main_delivery,
        main_artifact,
    ) {
        (None, None, None, None) => {
            if foreign_delivery_in_suffix {
                bail!("generic review normal path 含其它未提交 ReviewDelivered");
            }
            Ok(GenericDeliveryStorageState::Normal {
                main_sha,
                ledger_bytes: working_ledger,
            })
        }
        (Some(delivery), Some(bytes), None, None) => {
            if foreign_delivery_in_suffix {
                bail!("generic review recovery suffix 含其它 ReviewDelivered");
            }
            if working_ledger.len() <= main_ledger.len() {
                bail!("generic review recovery ledger 缺 strict suffix");
            }
            Ok(GenericDeliveryStorageState::Recoverable {
                delivery,
                bytes,
                ledger_bytes: working_ledger,
                main_sha,
            })
        }
        (Some(delivery), Some(bytes), Some(committed), Some(committed_bytes)) => {
            if !same_event_record(&delivery, &committed) || bytes != committed_bytes {
                bail!("generic review committed/working event 或 artifact 漂移");
            }
            Ok(GenericDeliveryStorageState::Committed {
                delivery,
                bytes,
                ledger_bytes: working_ledger,
                main_sha,
            })
        }
        _ => bail!(
            "generic review event/artifact storage split-brain；须人工核账，禁止自动补 event 或删除 artifact"
        ),
    }
}

/// Install, account, and commit one exact schema-3 generic review.
pub fn deliver_generic_review(
    root: &Path,
    task: &str,
    attempt: &str,
    harness: &str,
    wake_id: &str,
) -> Result<GenericReviewDeliveryOutcome> {
    let identity = GenericReviewIdentity::new(task, attempt, harness, wake_id)?;
    if harness == "local" {
        bail!("generic review 禁止 implementer local 自审");
    }
    let round = crate::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let before = orch_core::read_ledger(&ledger_path)?;
    crate::attempt::reject_bad_lines(&before)?;
    if before.events.iter().any(|event| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.task_id.as_deref() == Some(task)
            && payload_string(event, "attemptId") == Some(attempt)
    }) {
        bail!("generic review deliver 拒绝 root verdict 后的 receipt/artifact 写入");
    }
    let initial_storage =
        generic_delivery_storage_state(root, &round, &identity, &before.events)?;
    if matches!(initial_storage, GenericDeliveryStorageState::Normal { .. }) {
        let wake = exact_wake(&before.events, &round, &identity)?;
        if payload_string(wake, "method") != Some("unified-channel-v1") {
            bail!("live generic review deliver 只接受 unified channel request");
        }
        if wake
            .payload
            .as_ref()
            .and_then(|payload| payload.get("providerKind"))
            .is_some_and(|value| !value.is_null())
        {
            let _ = crate::wake::reconcile_backend_receipt(root, &round, wake_id)?;
        }
        let _ = crate::wake::reconcile_managed_wake_termination(root, &round, wake_id)?;
    }
    crate::close::with_protocol_transition(root, "generic review deliver", || {
        let read = orch_core::read_ledger(&ledger_path)?;
        crate::attempt::reject_bad_lines(&read)?;
        let active = crate::plan::require_active_round_ir(root, &round, &read.events)?;
        if active.candidate.schema_version != crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
            bail!("generic review deliver 只允许 schema 3 round");
        }
        if read.events.iter().any(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
        }) {
            bail!("generic review deliver 拒绝 root verdict 后的 artifact 写入");
        }
        let storage =
            generic_delivery_storage_state(root, &round, &identity, &read.events)?;
        let request = exact_request(&read.events, &round, &identity)?;
        let wake = exact_wake(&read.events, &round, &identity)?;
        if payload_string(wake, "method") != Some("unified-channel-v1") {
            bail!("live generic review deliver 只接受 unified channel request");
        }
        let reviewed_head = payload_string(request, "reviewedHead")
            .filter(|value| canonical_sha(value, 40))
            .context("generic ReviewRequested.reviewedHead 非 full SHA")?;
        let leases = read
            .events
            .iter()
            .filter(|event| {
                event.kind == "WorkspaceLeased"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round.as_str())
                    && event.task_id.as_deref() == Some(task)
                    && payload_string(event, "attemptId") == Some(attempt)
                    && payload_string(event, "role") == Some(GENERIC_REVIEW_ROLE)
                    && payload_string(event, "agent") == Some(harness)
                    && payload_string(event, "wakeId") == Some(wake_id)
                    && payload_string(event, "reviewedHead") == Some(reviewed_head)
            })
            .collect::<Vec<_>>();
        let [lease] = leases.as_slice() else {
            bail!("generic review deliver 缺唯一 exact WorkspaceLeased");
        };
        let worktree_rel = lease
            .payload
            .as_ref()
            .and_then(|payload| payload.get("paths"))
            .and_then(|paths| paths.get("worktree"))
            .and_then(serde_json::Value::as_str)
            .context("generic review deliver lease 缺 worktree")?;
        let request_paths = validate_channel_request_facts(
            request,
            wake,
            &identity,
            &round,
            reviewed_head,
            Path::new(worktree_rel),
        )?;
        if fs::canonicalize(root)? != request_paths.historical_root {
            bail!("live generic review repository root 与 immutable request 漂移");
        }
        let terminal = exact_terminal(&read.events, &round, &identity)?
            .context("generic review 尚无 trusted managed terminal")?;
        if !generic_review_terminal_closed_v1(terminal) || !terminal_answered(terminal) {
            bail!("generic review 非 trusted answered terminal，禁止 artifact delivery");
        }
        crate::wake::validate_existing_channel_action_binding(wake, terminal)?;
        let receipt = exact_backend_receipt(&read.events, &round, &identity)?;
        if let Some(receipt) = receipt {
            crate::wake::validate_existing_channel_action_binding(wake, receipt)?;
        } else if wake
            .payload
            .as_ref()
            .and_then(|payload| payload.get("providerKind"))
            .is_some_and(|value| !value.is_null())
        {
            bail!("generic review 缺 driver-declared backend receipt");
        }
        if let Some(receipt) = receipt {
            let request_position = unique_position(&read.events, &request.event_id)?;
            let receipt_position = unique_position(&read.events, &receipt.event_id)?;
            let terminal_position = unique_position(&read.events, &terminal.event_id)?;
            if !(request_position < receipt_position && receipt_position < terminal_position) {
                bail!("generic review backend receipt 未位于 request 与 terminal 之间");
            }
        }
        generic_review_terminal_release_closed_v1(
            &read.events,
            &round,
            task,
            attempt,
            harness,
            wake_id,
        )?;
        let canonical_rel =
            crate::verify::canonical_review_artifact_relpath(&round, attempt, "review", harness);
        if let GenericDeliveryStorageState::Committed {
            delivery,
            bytes,
            ledger_bytes,
            main_sha,
        } = &storage
        {
            let replay_sha = hex::encode(Sha256::digest(bytes));
            let inbox = root.join(crate::wake::review_inbox_relpath(
                &round, attempt, "review", harness,
            )?);
            if payload_string(terminal, "outputPath") != Some(inbox.to_string_lossy().as_ref())
                || payload_string(terminal, "outputSha256") != Some(replay_sha.as_str())
            {
                bail!("generic review replay terminal output path/digest 漂移");
            }
            validate_delivery_event(
                delivery,
                &identity,
                &round,
                reviewed_head,
                request,
                terminal,
                &canonical_rel,
                bytes,
            )?;
            let replay_read = orch_core::read_ledger(&ledger_path)?;
            crate::attempt::reject_bad_lines(&replay_read)?;
            let GenericDeliveryStorageState::Committed {
                delivery: observed_delivery,
                bytes: observed_bytes,
                ledger_bytes: observed_ledger,
                main_sha: observed_main,
            } = generic_delivery_storage_state(root, &round, &identity, &replay_read.events)?
            else {
                bail!("generic review committed replay 在返回前不再是 committed state");
            };
            if observed_main != *main_sha
                || observed_ledger != *ledger_bytes
                || observed_bytes != *bytes
                || !same_event_record(&observed_delivery, delivery)
            {
                bail!("generic review committed replay census/readback 漂移");
            }
            return Ok(GenericReviewDeliveryOutcome {
                path: canonical_rel,
                commit_sha: main_sha.clone(),
                delivery_event_id: delivery.event_id.clone(),
                replayed: true,
            });
        }
        if let GenericDeliveryStorageState::Recoverable {
            delivery,
            bytes,
            ledger_bytes,
            main_sha,
        } = &storage
        {
            let replay_sha = hex::encode(Sha256::digest(bytes));
            let inbox = root.join(crate::wake::review_inbox_relpath(
                &round, attempt, "review", harness,
            )?);
            if payload_string(terminal, "outputPath") != Some(inbox.to_string_lossy().as_ref())
                || payload_string(terminal, "outputSha256") != Some(replay_sha.as_str())
            {
                bail!("generic review recovery terminal output path/digest 漂移");
            }
            validate_delivery_event(
                delivery,
                &identity,
                &round,
                reviewed_head,
                request,
                terminal,
                &canonical_rel,
                bytes,
            )?;
            let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
            let expected_paths = [
                (ledger_rel.clone(), ledger_bytes.clone()),
                (canonical_rel.clone(), bytes.clone()),
            ];
            let commit_sha = crate::ledger::commit_scoped_accounting_paths_at_main(
                root,
                &round,
                main_sha,
                &expected_paths,
                &format!("state({round}): recover generic review {task}/{attempt}/{harness}"),
            )?;
            let committed = committed_regular_blob_bytes(root, &commit_sha, &canonical_rel)?
                .context("generic review recovery commit 缺 regular artifact")?;
            let committed_ledger = crate::gitx::show_bytes(root, &commit_sha, &ledger_rel)?;
            let committed_events = parse_committed_event_records(&committed_ledger)?;
            let committed_deliveries = committed_events
                .iter()
                .filter(|event| event.event_id == delivery.event_id)
                .collect::<Vec<_>>();
            let [committed_delivery] = committed_deliveries.as_slice() else {
                bail!("generic review recovery commit eventId 不唯一");
            };
            if committed.as_slice() != bytes.as_slice()
                || !same_event_record(committed_delivery, delivery)
            {
                bail!("generic review recovery commit readback 漂移");
            }
            return Ok(GenericReviewDeliveryOutcome {
                path: canonical_rel,
                commit_sha,
                delivery_event_id: delivery.event_id.clone(),
                replayed: false,
            });
        }
        let (normal_main, normal_ledger) = match &storage {
            GenericDeliveryStorageState::Normal {
                main_sha,
                ledger_bytes,
            } => (main_sha.clone(), ledger_bytes.clone()),
            _ => bail!("generic review storage state 未建模"),
        };
        let inbox_rel = crate::wake::review_inbox_relpath(&round, attempt, "review", harness)?;
        let inbox = root.join(&inbox_rel);
        let bytes = crate::verify::optional_review_artifact_bytes(
            root,
            &inbox,
            "staged generic review inbox",
        )?
        .context("staged generic review inbox 缺失")?;
        let checked = check_artifact(&bytes, &identity, &round, reviewed_head)?;
        let actual_sha = hex::encode(Sha256::digest(&bytes));
        if payload_string(terminal, "outputPath") != Some(inbox.to_string_lossy().as_ref())
            || payload_string(terminal, "outputSha256") != Some(actual_sha.as_str())
        {
            bail!("generic review terminal output path/digest 与 inbox bytes 不一致");
        }
        install_new_canonical(root, &root.join(&canonical_rel), &bytes)?;
        let delivery = crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some(task),
            Some(&round),
            serde_json::json!({
                "attemptId": attempt,
                "role": "review",
                "agent": harness,
                "harness": harness,
                "wakeId": wake_id,
                "reviewedHead": reviewed_head,
                "requestEventId": request.event_id,
                "terminalEventId": terminal.event_id,
                "path": canonical_rel,
                "sha256": actual_sha,
                "bytes": bytes.len() as u64,
                "bodyLen": checked.body_len,
                "verdict": checked.verdict,
            }),
        );
        validate_delivery_event(
            &delivery,
            &identity,
            &round,
            reviewed_head,
            request,
            terminal,
            &canonical_rel,
            &bytes,
        )?;
        let delivery_event_id = delivery.event_id.clone();
        let mut expected_ledger = normal_ledger;
        serde_json::to_writer(&mut expected_ledger, &delivery)?;
        expected_ledger.push(b'\n');
        crate::ledger::append(root, &round, &[delivery])?;
        let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
        let ledger_bytes = fs::read(root.join(&ledger_rel))?;
        if ledger_bytes != expected_ledger {
            bail!("generic review delivery append 不是 captured ledger 的 exact one-event suffix");
        }
        let wal = root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl"));
        require_existing_real_parent_chain(root, &wal, "generic review WAL after append")?;
        ensure_single_link(&wal, "generic review WAL after delivery append")?;
        if fs::read(&wal)? != ledger_bytes {
            bail!("generic review ledger/WAL 在 delivery append 后漂移");
        }
        let expected_paths = [
            (ledger_rel.clone(), ledger_bytes),
            (canonical_rel.clone(), bytes.clone()),
        ];
        let commit_sha = crate::ledger::commit_scoped_accounting_paths_at_main(
            root,
            &round,
            &normal_main,
            &expected_paths,
            &format!("state({round}): deliver generic review {task}/{attempt}/{harness}"),
        )?;
        Ok(GenericReviewDeliveryOutcome {
            path: canonical_rel,
            commit_sha,
            delivery_event_id,
            replayed: false,
        })
    })
}

/// Validate every initiated schema-3 review as facts only, returning the
/// substantive artifact bindings without counting votes or applying vetoes.
pub fn validate_generic_review_facts(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task: &str,
    attempt: &str,
    reviewed_head: &str,
    main_sha: &str,
) -> Result<Vec<crate::verify::ReviewBinding>> {
    validate_generic_review_facts_for_verdict(
        root,
        events,
        round,
        task,
        attempt,
        reviewed_head,
        main_sha,
        crate::verify::RootVerdict::Pass,
        false,
    )
}

struct GenericReviewRequestPaths {
    historical_root: PathBuf,
}

fn required_event_string<'a>(
    event: &'a orch_core::EventRecord,
    key: &str,
    label: &str,
) -> Result<&'a str> {
    payload_string(event, key)
        .filter(|value| !value.trim().is_empty() && value.trim() == *value)
        .with_context(|| format!("{label} 缺 non-blank {key}"))
}

fn validate_channel_request_facts(
    request: &orch_core::EventRecord,
    wake: &orch_core::EventRecord,
    identity: &GenericReviewIdentity,
    round: &str,
    reviewed_head: &str,
    worktree_rel: &Path,
) -> Result<GenericReviewRequestPaths> {
    let continuation = format!(
        "review:{round}:{}:{}:review:{}",
        identity.task, identity.attempt, identity.harness
    );
    for event in [wake, request] {
        if required_event_string(event, "method", "generic channel action")? != "unified-channel-v1"
            || required_event_string(event, "action", "generic channel action")? != "review"
            || required_event_string(event, "agent", "generic channel action")? != identity.harness
            || required_event_string(event, "harness", "generic channel action")?
                != identity.harness
            || required_event_string(event, "wakeId", "generic channel action")? != identity.wake_id
            || required_event_string(event, "continuationId", "generic channel action")?
                != continuation
        {
            bail!("generic unified request/wake action identity 漂移");
        }
    }
    if payload_string(request, "reviewedHead") != Some(reviewed_head)
        || payload_string(request, "fixedHead") != Some(reviewed_head)
        || payload_string(wake, "fixedHead") != Some(reviewed_head)
    {
        bail!("generic unified request/wake fixed HEAD 漂移");
    }
    let wake_binding = crate::wake::channel_action_binding_from_wake(wake)?
        .context("generic WakeIssued 缺 unified channel binding")?;
    let request_binding = crate::wake::channel_action_binding_from_wake(request)?
        .context("generic ReviewRequested 缺 mirrored channel binding")?;
    if request_binding != wake_binding {
        bail!("generic ReviewRequested 与 WakeIssued channel binding 漂移");
    }
    for key in [
        "executable",
        "requestMessageSha256",
        "renderedMessageSha256",
    ] {
        if required_event_string(request, key, "generic ReviewRequested")?
            != required_event_string(wake, key, "generic WakeIssued")?
        {
            bail!("generic ReviewRequested 与 WakeIssued {key} 漂移");
        }
    }
    if payload_string(wake, "requestMessageSha256") != payload_string(wake, "requestDigest")
        || !payload_string(wake, "renderedMessageSha256")
            .is_some_and(|value| canonical_sha(value, 64))
        || !Path::new(required_event_string(
            wake,
            "executable",
            "generic WakeIssued",
        )?)
        .is_absolute()
    {
        bail!("generic unified message/executable binding 非 canonical");
    }
    let cwd = required_event_string(request, "invocationCwd", "generic ReviewRequested")?;
    let config = required_event_string(request, "configDigest", "generic ReviewRequested")?;
    let request_digest =
        required_event_string(request, "requestDigest", "generic ReviewRequested")?;
    let attachments = required_event_string(
        request,
        "attachmentManifestSha256",
        "generic ReviewRequested",
    )?;
    let binding = GenericReviewLiveBindingV1::new(
        identity.clone(),
        reviewed_head,
        cwd,
        request_digest,
        config,
        attachments,
    )?;
    if !binding.matches(
        required_event_string(wake, "fixedHead", "generic WakeIssued")?,
        required_event_string(wake, "invocationCwd", "generic WakeIssued")?,
        required_event_string(wake, "requestDigest", "generic WakeIssued")?,
        required_event_string(wake, "configDigest", "generic WakeIssued")?,
        required_event_string(wake, "attachmentManifestSha256", "generic WakeIssued")?,
    ) {
        bail!("generic unified live binding 未逐字段匹配");
    }
    let cwd_path = PathBuf::from(cwd);
    let selection = required_event_string(request, "cwdSelection", "generic ReviewRequested")?;
    if payload_string(wake, "cwdSelection") != Some(selection) {
        bail!("generic request/wake cwdSelection 漂移");
    }
    let historical_root = match selection {
        "project-root" => cwd_path.clone(),
        "target-worktree" if cwd_path.ends_with(worktree_rel) => cwd_path
            .ancestors()
            .nth(worktree_rel.components().count())
            .map(Path::to_path_buf)
            .context("generic target-worktree cwd 无法还原 repository root")?,
        "target-worktree" => bail!("generic target-worktree cwd 未绑定 lease path"),
        _ => bail!("generic cwdSelection 未建模"),
    };
    Ok(GenericReviewRequestPaths { historical_root })
}

fn validate_historical_b319_request_facts(
    root: &Path,
    events: &[orch_core::EventRecord],
    request: &orch_core::EventRecord,
    wake: &orch_core::EventRecord,
    identity: &GenericReviewIdentity,
    round: &str,
    reviewed_head: &str,
    wake_position: usize,
    worktree_rel: &Path,
) -> Result<GenericReviewRequestPaths> {
    if round != "r83" || identity.task != "B319" || identity.attempt != "B319-A0001" {
        bail!("non-unified generic review 只允许 recorded r83/B319 historical replay");
    }
    let roots = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "attemptId") == Some(identity.attempt.as_str())
        })
        .collect::<Vec<_>>();
    let recorded = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "TaskRecorded"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "postMergeGates") == Some("all-green")
        })
        .collect::<Vec<_>>();
    let ([(root_position, _)], [(recorded_position, _)]) = (roots.as_slice(), recorded.as_slice())
    else {
        bail!("historical r83/B319 decoder 要求唯一 root 与 TaskRecorded");
    };
    if root_position >= recorded_position {
        bail!("historical r83/B319 root/recorded 顺序漂移");
    }
    let required = |event, key, label| required_event_string(event, key, label);
    for key in [
        "bootstrapSourceSha256",
        "requestMessageSha256",
        "attachmentManifestSha256",
        "invocationCwd",
        "bootstrapRepositoryRoot",
        "fixedHead",
        "bootstrapExecutable",
        "effectiveInvocationSha256",
    ] {
        if required(request, key, "historical ReviewRequested")?
            != required(wake, key, "historical WakeIssued")?
        {
            bail!("historical generic request/wake field 漂移: {key}");
        }
    }
    let source_sha = required(request, "bootstrapSourceSha256", "historical request")?;
    let request_sha = required(request, "requestMessageSha256", "historical request")?;
    let attachment_sha = required(request, "attachmentManifestSha256", "historical request")?;
    let cwd = required(request, "invocationCwd", "historical request")?;
    let repository_root = required(request, "bootstrapRepositoryRoot", "historical request")?;
    let fixed_head = required(request, "fixedHead", "historical request")?;
    let executable = required(request, "bootstrapExecutable", "historical request")?;
    let effective = required(request, "effectiveInvocationSha256", "historical request")?;
    if !canonical_sha(source_sha, 64)
        || !canonical_sha(request_sha, 64)
        || !canonical_sha(attachment_sha, 64)
        || fixed_head != reviewed_head
        || !Path::new(cwd).is_absolute()
        || !Path::new(repository_root).is_absolute()
        || !Path::new(executable).is_absolute()
        || !canonical_sha(effective, 64)
    {
        bail!("historical generic snapshot 非 canonical");
    }
    for key in [
        "bootstrapRequestedProvider",
        "bootstrapRequestedModel",
        "bootstrapRequestedEffort",
    ] {
        if request
            .payload
            .as_ref()
            .and_then(|payload| payload.get(key))
            != wake.payload.as_ref().and_then(|payload| payload.get(key))
        {
            bail!("historical generic requested tuple 漂移: {key}");
        }
    }
    let dispatches = events[..wake_position]
        .iter()
        .filter(|event| {
            event.kind == "DispatchIssued"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(identity.task.as_str())
                && payload_string(event, "attemptId") == Some(identity.attempt.as_str())
                && payload_string(event, "agent") == Some("local")
        })
        .collect::<Vec<_>>();
    let [dispatch] = dispatches.as_slice() else {
        bail!("historical generic snapshot 缺唯一 local DispatchIssued");
    };
    let base_sha = payload_string(dispatch, "baseSha")
        .filter(|value| canonical_sha(value, 40))
        .context("historical DispatchIssued.baseSha 非 canonical")?;
    if historical_bootstrap_source_digest(root, base_sha)? != source_sha {
        bail!("historical bootstrap invocation source digest 漂移");
    }
    let historical_root = PathBuf::from(repository_root);
    let expected_cwd = historical_root.join(worktree_rel);
    if Path::new(cwd) != expected_cwd {
        bail!("historical generic cwd 未绑定 target-worktree lease path");
    }
    Ok(GenericReviewRequestPaths { historical_root })
}

/// Validate generic review facts under one explicit root verdict, preserving
/// the public validator's strict PASS semantics for every other caller. A
/// prospective root is still a live write and retains the current-cwd check.
pub(crate) fn validate_generic_review_facts_for_verdict(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task: &str,
    attempt: &str,
    reviewed_head: &str,
    main_sha: &str,
    verdict: crate::verify::RootVerdict,
    prospective_root: bool,
) -> Result<Vec<crate::verify::ReviewBinding>> {
    if !canonical_sha(reviewed_head, 40) || !canonical_sha(main_sha, 40) {
        bail!("generic review validation 要求 full reviewed/main SHA");
    }
    let roots = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    if roots.len() > 1 {
        bail!("generic review attempt 存在重复 canonical root verdict");
    }
    let boundary = roots.first().copied().unwrap_or(events.len());
    let retired = [
        "ReviewPanelSelected",
        "ReviewSeatRouted",
        "ReviewSeatTerminated",
        "ReviewPanelClosed",
        "ReviewFallbackSelected",
        "ReviewSeatSubstituted",
        "NongateReviewDelivered",
    ];
    if events[..boundary].iter().any(|event| {
        event.task_id.as_deref() == Some(task)
            && payload_string(event, "attemptId") == Some(attempt)
            && (retired.contains(&event.kind.as_str())
                || (matches!(event.kind.as_str(), "ReviewRequested" | "ReviewDelivered")
                    && payload_string(event, "role") != Some("review")))
    }) {
        bail!("schema 3 attempt 注入 legacy Formal/Panel/Quorum review fact");
    }

    let requests = events[..boundary]
        .iter()
        .filter(|event| {
            event.kind == "ReviewRequested"
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
        })
        .collect::<Vec<_>>();
    let request_wake_ids = requests
        .iter()
        .map(|event| {
            payload_string(event, "wakeId")
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .context("generic ReviewRequested 缺 wakeId")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if request_wake_ids.len() != requests.len() {
        bail!("generic ReviewRequested wakeId 重复");
    }
    let continuation_prefix = format!("review:{round}:{task}:{attempt}:review:");
    let generic_wakes = events[..boundary]
        .iter()
        .filter(|event| {
            event.kind == "WakeIssued"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
                && (event
                    .payload
                    .as_ref()
                    .is_some_and(|payload| payload.get("bootstrapSourceSha256").is_some())
                    || payload_string(event, "continuationId")
                        .is_some_and(|value| value.starts_with(&continuation_prefix)))
        })
        .collect::<Vec<_>>();
    let generic_wake_ids = generic_wakes
        .iter()
        .map(|event| {
            payload_string(event, "wakeId")
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .context("generic WakeIssued 缺 wakeId")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if generic_wake_ids.len() != generic_wakes.len() || generic_wake_ids != request_wake_ids {
        bail!("generic WakeIssued 与 ReviewRequested identity 集不相等");
    }
    let review_leases = events[..boundary]
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceLeased"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task)
                && payload_string(event, "attemptId") == Some(attempt)
                && payload_string(event, "role") == Some("review")
                && !crate::sites::lease_is_pre_spawn_rejected(&events[..boundary], event)
        })
        .collect::<Vec<_>>();
    let lease_wake_ids = review_leases
        .iter()
        .map(|event| {
            payload_string(event, "wakeId")
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .context("generic WorkspaceLeased 缺 wakeId")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if lease_wake_ids.len() != review_leases.len() || lease_wake_ids != request_wake_ids {
        bail!("generic WorkspaceLeased 与 ReviewRequested identity 集不相等");
    }
    let mut harnesses = BTreeSet::new();
    let mut wakes = BTreeSet::new();
    let mut live_requests = Vec::new();
    let mut live_terminals = Vec::new();
    let mut consumed_deliveries = BTreeSet::new();
    let mut bindings = Vec::new();
    for request in requests {
        let identity = request_identity(request)?;
        if identity.task != task
            || identity.attempt != attempt
            || identity.harness == "local"
            || !harnesses.insert(identity.harness.clone())
            || !wakes.insert(identity.wake_id.clone())
        {
            bail!("generic review request identity 重复/自审/漂移");
        }
        live_requests.push(identity.clone());
        let prefix = &events[..boundary];
        let request = exact_request(prefix, round, &identity)?;
        let wake = exact_wake(prefix, round, &identity)?;
        let request_position = unique_position(events, &request.event_id)?;
        let wake_position = unique_position(events, &wake.event_id)?;
        let collect_positions = events[..wake_position]
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "ReportCollectCompleted"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round)
                    && event.task_id.as_deref() == Some(task)
                    && payload_string(event, "attemptId") == Some(attempt)
                    && payload_string(event, "branchSha") == Some(reviewed_head)
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        let [collect_position] = collect_positions.as_slice() else {
            bail!("generic review request 缺唯一 immutable collect predecessor");
        };
        if !(*collect_position < wake_position && wake_position < request_position) {
            bail!("generic review 要求 collect < WakeIssued < ReviewRequested");
        }

        let leases = events[..request_position]
            .iter()
            .filter(|event| {
                event.kind == "WorkspaceLeased"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round)
                    && event.task_id.as_deref() == Some(task)
                    && payload_string(event, "attemptId") == Some(attempt)
                    && payload_string(event, "role") == Some("review")
                    && payload_string(event, "agent") == Some(identity.harness.as_str())
                    && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
                    && payload_string(event, "reviewedHead") == Some(reviewed_head)
            })
            .collect::<Vec<_>>();
        let [lease] = leases.as_slice() else {
            bail!("generic review 缺唯一 WorkspaceLeased");
        };
        let worktree_rel = lease
            .payload
            .as_ref()
            .and_then(|payload| payload.get("paths"))
            .and_then(|paths| paths.get("worktree"))
            .and_then(serde_json::Value::as_str)
            .context("generic review WorkspaceLeased 缺 worktree path")?;
        let relative = Path::new(worktree_rel);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir
                        | std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            bail!("generic review WorkspaceLeased worktree path 非 canonical relative");
        }
        let request_paths = if payload_string(wake, "method") == Some("unified-channel-v1") {
            validate_channel_request_facts(
                request,
                wake,
                &identity,
                round,
                reviewed_head,
                relative,
            )?
        } else {
            validate_historical_b319_request_facts(
                root,
                events,
                request,
                wake,
                &identity,
                round,
                reviewed_head,
                wake_position,
                relative,
            )?
        };
        if roots.is_empty() || prospective_root {
            let current_root = std::fs::canonicalize(root)?;
            if current_root != request_paths.historical_root {
                bail!("live generic review repository root 与 immutable request 漂移");
            }
        }

        let unified = payload_string(wake, "method") == Some("unified-channel-v1");
        let terminal = exact_terminal(prefix, round, &identity)?;
        let rejection = exact_rejection(prefix, round, &identity)?;
        let receipt = exact_backend_receipt(prefix, round, &identity)?;
        if let Some(quarantine) = rejection.filter(|event| is_review_quarantine(event)) {
            if verdict != crate::verify::RootVerdict::Blocked {
                bail!("review quarantine is only valid for root BLOCKED");
            }
            canonical_review_quarantine(quarantine, events, round)?;
        }
        let deliveries = events[..boundary]
            .iter()
            .filter(|event| {
                event.kind == "ReviewDelivered"
                    && event.task_id.as_deref() == Some(task)
                    && payload_string(event, "attemptId") == Some(attempt)
                    && payload_string(event, "harness") == Some(identity.harness.as_str())
                    && payload_string(event, "wakeId") == Some(identity.wake_id.as_str())
            })
            .collect::<Vec<_>>();
        if unified {
            if wake
                .payload
                .as_ref()
                .and_then(|payload| payload.get("providerKind"))
                .is_some_and(|value| !value.is_null())
                && receipt.is_none()
                && rejection.is_none()
                && !receiptless_terminal_is_closed_zero_vote(terminal, &deliveries)
            {
                bail!("generic unified review 缺 driver-declared backend receipt");
            }
            if let Some(receipt) = receipt {
                crate::wake::validate_existing_channel_action_binding(wake, receipt)?;
            }
        }
        let (completion, answered, terminal_class) = if let Some(terminal) = terminal {
            if unified {
                crate::wake::validate_existing_channel_action_binding(wake, terminal)?;
            }
            let projection_recovered = !generic_review_terminal_closed_v1(terminal)
                && bootstrap_projection_mismatch_closed_for_nonpass(
                    prefix, round, &identity, request, wake, lease, terminal, receipt, verdict,
                )?;
            if !generic_review_terminal_closed_v1(terminal) && !projection_recovered {
                bail!("generic ManagedWakeTerminated 非 trusted closed terminal");
            }
            if unified && !projection_recovered {
                if let Some(receipt) = receipt {
                    let receipt_position = unique_position(events, &receipt.event_id)?;
                    let terminal_position = unique_position(events, &terminal.event_id)?;
                    if !(request_position < receipt_position
                        && receipt_position < terminal_position)
                    {
                        bail!("generic backend receipt 未位于 request 与 terminal 之间");
                    }
                }
                generic_review_terminal_release_closed_v1(
                    prefix,
                    round,
                    &identity.task,
                    &identity.attempt,
                    &identity.harness,
                    &identity.wake_id,
                )?;
            }
            let class = if projection_recovered {
                GenericReviewLiveTerminalV1::Failed(identity.clone())
            } else {
                match payload_string(terminal, "state") {
                    Some("answered") => GenericReviewLiveTerminalV1::Answered(identity.clone()),
                    Some("failed") => GenericReviewLiveTerminalV1::Failed(identity.clone()),
                    Some("timedOut") => GenericReviewLiveTerminalV1::TimedOut(identity.clone()),
                    Some("empty") => GenericReviewLiveTerminalV1::Empty(identity.clone()),
                    Some("canceled") => GenericReviewLiveTerminalV1::Canceled(identity.clone()),
                    _ => bail!("generic ManagedWakeTerminated state 未建模"),
                }
            };
            (
                terminal,
                !projection_recovered && terminal_answered(terminal),
                class,
            )
        } else if !unified {
            let receipt = receipt.context("historical B319 generic review 缺 accepted receipt")?;
            if deliveries.is_empty() {
                bail!("historical accepted receipt 只有与 substantive delivery 组合才构成终态");
            }
            (
                receipt,
                true,
                GenericReviewLiveTerminalV1::Answered(identity.clone()),
            )
        } else if let Some(rejection) = rejection {
            (
                rejection,
                false,
                GenericReviewLiveTerminalV1::Rejected(identity.clone()),
            )
        } else {
            bail!("generic review request 尚无 trusted terminal/rejection");
        };
        live_terminals.push(terminal_class);
        let completion_position = unique_position(events, &completion.event_id)?;
        if let Some(rejection) = rejection {
            let rejection_position = unique_position(events, &rejection.event_id)?;
            if rejection.event_id != completion.event_id
                && rejection_position >= completion_position
            {
                bail!("generic ActionRejected 不得晚于 trusted terminal/accepted receipt");
            }
        }
        if request_position >= completion_position || completion_position >= boundary {
            bail!("generic review completion 未位于 request 与 root 之间");
        }
        if !answered {
            if !deliveries.is_empty() {
                bail!("non-answered generic review 不得携 artifact delivery");
            }
            continue;
        }
        let [delivery] = deliveries.as_slice() else {
            bail!("answered generic review 要求唯一 ReviewDelivered");
        };
        let delivery_position = unique_position(events, &delivery.event_id)?;
        if completion_position >= delivery_position || delivery_position >= boundary {
            bail!("generic review delivery 未位于 completion 与 root 之间");
        }
        let expected_head = payload_string(request, "reviewedHead")
            .filter(|value| *value == reviewed_head)
            .context("generic request reviewedHead 漂移")?;
        let rel = crate::verify::canonical_review_artifact_relpath(
            round,
            attempt,
            "review",
            &identity.harness,
        );
        if delivery.actor != "runtime:orch"
            || delivery.round.as_deref() != Some(round)
            || payload_string(delivery, "role") != Some("review")
            || payload_string(delivery, "agent") != Some(identity.harness.as_str())
            || payload_string(delivery, "path") != Some(rel.as_str())
            || payload_string(delivery, "requestEventId") != Some(request.event_id.as_str())
            || payload_string(delivery, "terminalEventId") != Some(completion.event_id.as_str())
            || payload_string(delivery, "reviewedHead") != Some(expected_head)
        {
            bail!("generic ReviewDelivered provenance 漂移");
        }
        let committed = committed_regular_blob_bytes(root, main_sha, &rel)?
            .with_context(|| format!("committed generic review 缺 regular blob: {rel}"))?;
        let current = crate::verify::optional_review_artifact_bytes(
            root,
            &root.join(&rel),
            "generic review canonical",
        )?
        .context("generic review canonical 缺失")?;
        if current != committed {
            bail!("generic review current bytes 与 committed main 不一致");
        }
        validate_delivery_event(
            delivery,
            &identity,
            round,
            reviewed_head,
            request,
            completion,
            &rel,
            &committed,
        )?;
        let checked = check_artifact(&committed, &identity, round, reviewed_head)?;
        let sha = hex::encode(Sha256::digest(&committed));
        if let Some(terminal) = terminal.filter(|event| terminal_answered(event)) {
            let inbox_rel =
                crate::wake::review_inbox_relpath(round, attempt, "review", &identity.harness)?;
            let terminal_root = if roots.is_empty() || prospective_root {
                root
            } else {
                request_paths.historical_root.as_path()
            };
            if payload_string(terminal, "outputPath")
                != Some(terminal_root.join(inbox_rel).to_string_lossy().as_ref())
                || payload_string(terminal, "outputSha256") != Some(sha.as_str())
            {
                bail!("generic terminal output path/digest 未绑定 artifact");
            }
        }
        if payload_string(delivery, "sha256") != Some(sha.as_str())
            || payload_string(delivery, "verdict") != Some(checked.verdict.as_str())
            || delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bytes"))
                .and_then(serde_json::Value::as_u64)
                != Some(committed.len() as u64)
            || delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bodyLen"))
                .and_then(serde_json::Value::as_u64)
                != Some(checked.body_len)
        {
            bail!("generic ReviewDelivered bytes/verdict binding 漂移");
        }
        consumed_deliveries.insert(delivery.event_id.clone());
        bindings.push(crate::verify::ReviewBinding {
            path: rel,
            role: "review".to_string(),
            reviewer: identity.harness,
            verdict: checked.verdict,
            sha256: sha,
            bytes: committed.len() as u64,
            delivery_event_id: Some(delivery.event_id.clone()),
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        });
    }
    validate_generic_review_live_terminals_v1(&live_requests, &live_terminals)?;
    for event in events.iter().filter(|event| {
        event.kind == "ReviewDelivered"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task)
            && payload_string(event, "attemptId") == Some(attempt)
            && payload_string(event, "role") == Some("review")
    }) {
        let position = unique_position(events, &event.event_id)?;
        if position >= boundary || !consumed_deliveries.contains(&event.event_id) {
            bail!("generic review 存在 root 后或 orphan ReviewDelivered");
        }
    }
    let suffix = roots
        .first()
        .map(|position| &events[position.saturating_add(1)..])
        .unwrap_or(&[]);
    let mut cleanup_only = BTreeSet::new();
    if let Some(root_position) = roots.first().copied() {
        let mut position = root_position.saturating_add(1);
        while position < events.len() {
            let terminal = &events[position];
            if terminal.kind == "SiteRetired"
                && payload_string(terminal, "wakeId")
                    .is_some_and(|wake_id| request_wake_ids.contains(wake_id))
                && crate::verify::post_merge_site_retirement_is_authorized(
                    terminal,
                    &events[..position],
                    round,
                    task,
                )
            {
                cleanup_only.insert(terminal.event_id.clone());
                position += 1;
                continue;
            }
            if terminal.kind == "ManagedWakeTerminated"
                && payload_string(terminal, "wakeId")
                    .is_some_and(|wake_id| request_wake_ids.contains(wake_id))
            {
                let end = if events
                    .get(position + 1)
                    .is_some_and(|event| event.kind == "WorkspaceReleased")
                {
                    position + 2
                } else {
                    position + 1
                };
                if crate::verify::canonical_complete_seal_managed_terminal_suffix_v1(
                    &events[..position],
                    &events[position..end],
                    round,
                    task,
                )? {
                    cleanup_only.extend(
                        events[position..end]
                            .iter()
                            .map(|event| event.event_id.clone()),
                    );
                    position = end;
                    continue;
                }
            }
            position += 1;
        }
    }
    if suffix.iter().any(|event| {
        if cleanup_only.contains(&event.event_id) {
            return false;
        }
        let known_wake = payload_string(event, "wakeId")
            .or_else(|| payload_string(event, "actionId"))
            .is_some_and(|wake_id| request_wake_ids.contains(wake_id));
        let same_attempt = event.task_id.as_deref() == Some(task)
            && payload_string(event, "attemptId") == Some(attempt);
        let lifecycle = matches!(
            event.kind.as_str(),
            "WorkspaceLeased"
                | "WakeIssued"
                | "ReviewRequested"
                | "ReviewDelivered"
                | "ManagedWakeTerminated"
        ) || (event.kind == "AgentEventReceived"
            && payload_string(event, "agentEvent") == Some("wake-backend-receipt"))
            || (event.kind == "ActionRejected"
                && matches!(
                    payload_string(event, "operation"),
                    Some("wake" | "wake-backend-receipt" | "review-quarantine")
                ));
        known_wake || (same_attempt && lifecycle)
    }) {
        bail!("canonical root 后出现 generic review lifecycle fact");
    }
    bindings.sort_by(|left, right| left.reviewer.cmp(&right.reviewer));
    Ok(bindings)
}

#[cfg(test)]
mod quarantine_contract_tests {
    use super::*;

    // Sanitized events produced by the integration fixture; no native process existed.
    fn history() -> Vec<orch_core::EventRecord> {
        serde_json::from_str(include_str!("../tests/support_review_quarantine/history.json")).unwrap()
    }

    fn recognized(events: &[orch_core::EventRecord]) -> bool {
        let Some(q) = events.iter().find(|event| is_review_quarantine(event)) else { return false; };
        canonical_review_quarantine(q, events, "r83").is_ok_and(|value| value)
    }

    fn extra(kind: &str, wake: &str) -> orch_core::EventRecord {
        crate::ledger::event(kind, "runtime:orch", Some("B901"), Some("r83"),
            serde_json::json!({"attemptId":"B901-A0001", "wakeId":wake, "actionId":wake}))
    }

    #[test]
    fn quarantine_history_requires_exact_blocked_pair_receipt_and_unfinished_scope() {
        let original = history();
        assert!(recognized(&original));
        for index in 0..original.len() {
            let mut changed = original.clone(); changed.remove(index);
            assert!(!recognized(&changed), "missing event {index}");
        }
        let qi = original.len() - 2;
        let wake = payload_string(&original[qi], "actionId").unwrap();
        for kind in ["ManagedWakeTerminated", "WorkspaceReleased", "ReviewDelivered", "ActionRejected"] {
            let mut changed = original.clone(); changed.insert(qi, extra(kind, wake));
            assert!(!recognized(&changed), "cannot replace {kind}");
        }
        let mut shadowed = original.clone();
        let mut rejection = extra("ActionRejected", wake);
        rejection.payload.as_mut().unwrap()["wakeId"] = serde_json::json!("foreign");
        shadowed.insert(qi, rejection);
        assert!(!recognized(&shadowed), "wakeId cannot mask a conflicting actionId");
        for kind in ["WorkspaceLeased", "WakeIssued", "ReviewRequested", "AgentEventReceived", "ActionRejected"] {
            let mut changed = original.clone();
            let mut duplicate = changed.iter().find(|event| event.kind == kind).unwrap().clone();
            duplicate.event_id = ulid::Ulid::new().to_string();
            changed.insert(qi, duplicate);
            assert!(!recognized(&changed), "duplicate {kind} with a distinct eventId");
        }
        let mut separated = original.clone(); separated.insert(qi + 1, extra("GateExecuted", "another"));
        assert!(!recognized(&separated));
        let mut late = original.clone(); late.swap(qi, qi + 1);
        assert!(!recognized(&late));
        let mut legacy = original.clone(); legacy[0].payload.as_mut().unwrap()["contractSchemaVersion"] = serde_json::json!(2);
        assert!(!recognized(&legacy));
        let mut empty_path = original.clone(); empty_path[2].payload.as_mut().unwrap()["paths"]["worktree"] = serde_json::json!("");
        assert!(!recognized(&empty_path));
    }

    #[test]
    fn quarantine_envelope_and_root_fields_are_not_extensible_or_substitutable() {
        let original = history(); let qi = original.len() - 2; let ri = qi + 1;
        for key in original[qi].payload.as_ref().unwrap().as_object().unwrap().keys() {
            let mut missing = original.clone(); missing[qi].payload.as_mut().unwrap().as_object_mut().unwrap().remove(key);
            assert!(!recognized(&missing), "missing quarantine {key}");
        }
        for (key, value) in [
            ("exitCode", serde_json::json!(2)), ("alert", serde_json::json!(false)),
            ("attemptNo", serde_json::json!(2)), ("attemptId", serde_json::json!("B901-A0002")),
            ("reason", serde_json::json!("changed")), ("actionId", serde_json::json!("foreign")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut changed = original.clone(); changed[qi].payload.as_mut().unwrap()[key] = value;
            assert!(!recognized(&changed), "quarantine {key}");
        }
        for key in ["actor", "round", "task"] {
            let mut changed = original.clone();
            match key { "actor" => changed[qi].actor = "other".to_string(),
                "round" => changed[qi].round = Some("other".to_string()),
                _ => changed[qi].task_id = Some("other".to_string()) }
            assert!(!recognized(&changed), "quarantine {key}");
        }
        for (key, value) in [
            ("verdict", serde_json::json!("PASS")), ("verdict", serde_json::json!("FAIL")),
            ("reason", serde_json::json!("changed")), ("attemptNo", serde_json::json!(2)),
            ("headSha", serde_json::json!("f".repeat(40))), ("mainHeadSha", serde_json::json!("short")),
            ("extra", serde_json::json!(true)),
        ] {
            let mut changed = original.clone(); changed[ri].payload.as_mut().unwrap()[key] = value;
            assert!(!recognized(&changed), "root {key}");
        }
        for key in original[5].payload.as_ref().unwrap()["channelBinding"].as_object().unwrap().keys() {
            let mut changed = original.clone();
            changed[5].payload.as_mut().unwrap()["channelBinding"][key] = serde_json::json!("tampered");
            assert!(!recognized(&changed), "receipt binding {key}");
        }
    }

    #[test]
    fn quarantine_is_zero_vote_blocked_only_and_keeps_existing_identity_fences() {
        let events = history(); let root = events.last().unwrap().payload.as_ref().unwrap();
        let head = root["headSha"].as_str().unwrap(); let main = root["mainHeadSha"].as_str().unwrap();
        for verdict in [crate::verify::RootVerdict::Pass, crate::verify::RootVerdict::Fail] {
            assert!(validate_generic_review_facts_for_verdict(Path::new("/repo"), &events,
                "r83", "B901", "B901-A0001", head, main, verdict, false).is_err());
        }
        assert!(validate_generic_review_facts_for_verdict(Path::new("/repo"), &events,
            "r83", "B901", "B901-A0001", head, main, crate::verify::RootVerdict::Blocked, false).unwrap().is_empty());
        let q = &events[events.len() - 2]; let wake = payload_string(q, "actionId").unwrap();
        assert!(validate_generic_review_admission_v1(&events, "r83", "B901", "B901-A0002", "fresh", wake).is_err());
        assert!(validate_generic_review_admission_v1(&events, "r83", "B901", "B901-A0001", "held-native", "new-wake").is_err());
        assert!(validate_generic_review_admission_v1(&events, "r83", "B901", "B901-A0002", "fresh", "new-wake").is_ok());
        let load = crate::legacy::agent_inflight_load_from_events(&events, "r83").unwrap();
        assert_eq!(load.get("held-native").unwrap().len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_policy_terminal(answered: bool) -> orch_core::EventRecord {
        let payload = if answered {
            serde_json::json!({
                "agent": "agy", "wakeId": "wake-agy", "state": "answered",
                "completionReason": "natural-exit", "exactReason": "provider terminal",
                "outcomeClass": "DeliveredTerminal", "terminalSeen": true,
                "turnEnded": true, "exitedNaturally": true,
                "hardDeadlineReached": false, "managedScopeTerminated": true,
                "mechanicalTerminalAbsent": false, "cancelRequestId": null,
                "signals": [], "logBytesRead": 36,
                "outputPath": "/repo/review.md",
                "outputSha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "finalTextSha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            })
        } else {
            serde_json::json!({
                "agent": "agy", "wakeId": "wake-agy", "state": "failed",
                "completionReason": "natural-exit",
                "exactReason": "backend acceptance receipt absent before terminal reconciliation",
                "outcomeClass": "TruncatedNoTerminal", "terminalSeen": false,
                "turnEnded": false, "exitedNaturally": false,
                "hardDeadlineReached": false, "managedScopeTerminated": true,
                "mechanicalTerminalAbsent": false, "cancelRequestId": null,
                "signals": ["TERM"], "logBytesRead": 36,
                "outputPath": null, "outputSha256": null, "finalTextSha256": null
            })
        };
        crate::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some("B321"),
            Some("r83"),
            payload,
        )
    }

    #[test]
    fn receiptless_closed_nonanswer_is_zero_vote_but_answer_or_delivery_stays_closed() {
        let failed = receipt_policy_terminal(false);
        assert!(receiptless_terminal_is_closed_zero_vote(Some(&failed), &[]));

        let mut wrong_reason = failed.clone();
        wrong_reason.payload.as_mut().unwrap()["exactReason"] =
            serde_json::json!("different failure");
        assert!(!receiptless_terminal_is_closed_zero_vote(
            Some(&wrong_reason),
            &[]
        ));

        let answered = receipt_policy_terminal(true);
        assert!(!receiptless_terminal_is_closed_zero_vote(Some(&answered), &[]));
        let delivery = crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some("B321"),
            Some("r83"),
            serde_json::json!({}),
        );
        assert!(!receiptless_terminal_is_closed_zero_vote(
            Some(&failed),
            &[&delivery]
        ));
        assert!(!receiptless_terminal_is_closed_zero_vote(None, &[]));
    }

    #[test]
    fn receiptless_authenticated_timeout_closes_without_authorizing_an_answer() {
        let mut timeout = receipt_policy_terminal(false);
        let payload = timeout.payload.as_mut().unwrap();
        payload["state"] = serde_json::json!("timedOut");
        payload["completionReason"] = serde_json::json!("hard-deadline");
        payload["outcomeClass"] = serde_json::json!("StoppedByHardDeadline");
        payload["hardDeadlineReached"] = serde_json::json!(true);
        assert!(generic_review_terminal_closed_v1(&timeout));
        assert!(receiptless_terminal_is_closed_zero_vote(Some(&timeout), &[]));

        for (key, value) in [
            ("state", serde_json::json!("answered")),
            ("completionReason", serde_json::json!("natural-exit")),
            ("outcomeClass", serde_json::json!("TruncatedNoTerminal")),
            ("exactReason", serde_json::json!("different failure")),
            ("hardDeadlineReached", serde_json::json!(false)),
            ("exitedNaturally", serde_json::json!(true)),
            ("managedScopeTerminated", serde_json::json!(false)),
            ("terminalSeen", serde_json::json!(true)),
            ("turnEnded", serde_json::json!(true)),
            ("mechanicalTerminalAbsent", serde_json::json!(true)),
            ("cancelRequestId", serde_json::json!("cancel-other")),
            ("outputPath", serde_json::json!("/repo/review.md")),
            ("outputSha256", serde_json::json!("a".repeat(64))),
            ("finalTextSha256", serde_json::json!("b".repeat(64))),
        ] {
            let mut changed = timeout.clone();
            changed.payload.as_mut().unwrap()[key] = value;
            assert!(
                !receiptless_terminal_is_closed_zero_vote(Some(&changed), &[]),
                "receiptless timeout accepted changed {key}"
            );
        }
        let delivery = crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some("B321"),
            Some("r83"),
            serde_json::json!({}),
        );
        assert!(!receiptless_terminal_is_closed_zero_vote(
            Some(&timeout),
            &[&delivery]
        ));
        for key in [
            "managedScopeTerminated", "hardDeadlineReached", "exitedNaturally",
            "cancelRequestId", "outputPath", "outputSha256", "finalTextSha256",
        ] {
            let mut incomplete = timeout.clone();
            incomplete.payload.as_mut().unwrap().as_object_mut().unwrap().remove(key);
            assert!(!receiptless_terminal_is_closed_zero_vote(Some(&incomplete), &[]));
        }
    }

    fn generic_test_root(label: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("orch workspace root")
            .join("target/test-tmp")
            .join(format!("b321-generic-{label}-{}", ulid::Ulid::new()))
    }

    fn storage_git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn storage_commit(root: &Path, message: &str) -> String {
        storage_git(root, &["add", "-A"]);
        storage_git(
            root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch@test.invalid",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
        storage_git(root, &["rev-parse", "HEAD"])
    }

    fn storage_fixture(label: &str) -> (PathBuf, GenericReviewIdentity) {
        let root = generic_test_root(label);
        fs::create_dir_all(root.join("coordination/rounds/r83")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(root.join(".gitignore"), "coordination/runtime/\n").unwrap();
        storage_git(&root, &["init", "-q", "-b", "main"]);
        crate::ledger::append(
            &root,
            "r83",
            &[crate::ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some("r83"),
                serde_json::json!({"contractSchemaVersion": 3}),
            )],
        )
        .unwrap();
        storage_commit(&root, "base ledger");
        (
            root,
            GenericReviewIdentity::new("B321", "B321-A0001", "alpha", "wake-1").unwrap(),
        )
    }

    fn storage_events(root: &Path) -> Vec<orch_core::EventRecord> {
        let read = orch_core::read_ledger(&root.join("coordination/rounds/r83/events.jsonl"))
            .unwrap();
        assert!(read.bad_lines.is_empty());
        read.events
    }

    fn storage_delivery(identity: &GenericReviewIdentity) -> orch_core::EventRecord {
        let path = crate::verify::canonical_review_artifact_relpath(
            "r83",
            &identity.attempt,
            GENERIC_REVIEW_ROLE,
            &identity.harness,
        );
        crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some(&identity.task),
            Some("r83"),
            serde_json::json!({
                "attemptId": identity.attempt, "role": "review",
                "agent": identity.harness, "harness": identity.harness,
                "wakeId": identity.wake_id, "path": path
            }),
        )
    }

    fn storage_artifact_path(root: &Path, identity: &GenericReviewIdentity) -> PathBuf {
        root.join(crate::verify::canonical_review_artifact_relpath(
            "r83",
            &identity.attempt,
            GENERIC_REVIEW_ROLE,
            &identity.harness,
        ))
    }

    #[test]
    fn delivery_storage_census_accepts_three_states_and_rejects_splits() {
        let (root, identity) = storage_fixture("storage-three-states");
        assert!(matches!(
            generic_delivery_storage_state(&root, "r83", &identity, &storage_events(&root))
                .unwrap(),
            GenericDeliveryStorageState::Normal { .. }
        ));
        let artifact = storage_artifact_path(&root, &identity);
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"exact artifact\n").unwrap();
        crate::ledger::append(&root, "r83", &[storage_delivery(&identity)]).unwrap();
        let recoverable =
            generic_delivery_storage_state(&root, "r83", &identity, &storage_events(&root))
                .unwrap();
        let GenericDeliveryStorageState::Recoverable {
            bytes,
            ledger_bytes,
            main_sha,
            ..
        } = recoverable
        else {
            panic!("working event+artifact must be recoverable");
        };
        let ledger_rel = "coordination/rounds/r83/events.jsonl".to_string();
        let artifact_rel = artifact.strip_prefix(&root).unwrap().to_string_lossy().to_string();
        let expected = [
            (ledger_rel.clone(), ledger_bytes.clone()),
            (artifact_rel.clone(), bytes.clone()),
        ];
        let main_before = storage_git(&root, &["rev-parse", "refs/heads/main"]);
        crate::close::with_protocol_transition(&root, "wrong expected main", || {
            assert!(crate::ledger::commit_scoped_accounting_paths_at_main(
                &root,
                "r83",
                &"a".repeat(40),
                &expected,
                "must not commit on wrong main",
            )
            .is_err());
            Ok(())
        })
        .unwrap();
        assert_eq!(storage_git(&root, &["rev-parse", "refs/heads/main"]), main_before);
        fs::write(&artifact, b"drifted artifact\n").unwrap();
        crate::close::with_protocol_transition(&root, "drifted captured bytes", || {
            assert!(crate::ledger::commit_scoped_accounting_paths_at_main(
                &root,
                "r83",
                &main_sha,
                &expected,
                "must not commit drifted bytes",
            )
            .is_err());
            Ok(())
        })
        .unwrap();
        assert_eq!(storage_git(&root, &["rev-parse", "refs/heads/main"]), main_before);
        fs::write(&artifact, &bytes).unwrap();
        storage_commit(&root, "commit delivery pair");
        assert!(matches!(
            generic_delivery_storage_state(&root, "r83", &identity, &storage_events(&root))
                .unwrap(),
            GenericDeliveryStorageState::Committed { .. }
        ));
        let committed_head = storage_git(&root, &["rev-parse", "HEAD"]);
        let committed_ledger = fs::read(root.join("coordination/rounds/r83/events.jsonl")).unwrap();
        fs::remove_file(&artifact).unwrap();
        assert!(generic_delivery_storage_state(
            &root,
            "r83",
            &identity,
            &storage_events(&root),
        )
        .is_err());
        fs::write(&artifact, b"exact artifact\n").unwrap();
        let parent = storage_git(&root, &["rev-parse", "HEAD^"]);
        let parent_ledger = crate::gitx::show_bytes(
            &root,
            &parent,
            "coordination/rounds/r83/events.jsonl",
        )
        .unwrap();
        fs::write(root.join("coordination/rounds/r83/events.jsonl"), &parent_ledger).unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/r83.jsonl"),
            &parent_ledger,
        )
        .unwrap();
        assert!(generic_delivery_storage_state(
            &root,
            "r83",
            &identity,
            &storage_events(&root),
        )
        .is_err());
        fs::write(
            root.join("coordination/rounds/r83/events.jsonl"),
            &committed_ledger,
        )
        .unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/r83.jsonl"),
            &committed_ledger,
        )
        .unwrap();
        assert_eq!(storage_git(&root, &["rev-parse", "HEAD"]), committed_head);
        fs::remove_dir_all(&root).unwrap();

        let (artifact_only, identity) = storage_fixture("storage-artifact-only");
        let artifact = storage_artifact_path(&artifact_only, &identity);
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(artifact, b"orphan artifact\n").unwrap();
        assert!(generic_delivery_storage_state(
            &artifact_only,
            "r83",
            &identity,
            &storage_events(&artifact_only),
        )
        .is_err());
        fs::remove_dir_all(artifact_only).unwrap();

        let (event_only, identity) = storage_fixture("storage-event-only");
        crate::ledger::append(&event_only, "r83", &[storage_delivery(&identity)]).unwrap();
        assert!(generic_delivery_storage_state(
            &event_only,
            "r83",
            &identity,
            &storage_events(&event_only),
        )
        .is_err());
        fs::remove_dir_all(event_only).unwrap();

        let (foreign_suffix, identity) = storage_fixture("storage-foreign-delivery-suffix");
        let artifact = storage_artifact_path(&foreign_suffix, &identity);
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"exact artifact\n").unwrap();
        let other = GenericReviewIdentity::new(
            "B321",
            "B321-A0001",
            "beta",
            "wake-2",
        )
        .unwrap();
        let target_delivery = storage_delivery(&identity);
        let mut foreign_delivery = storage_delivery(&other);
        foreign_delivery.event_id = target_delivery.event_id.clone();
        let ledger_path = foreign_suffix.join("coordination/rounds/r83/events.jsonl");
        let mut ledger_bytes = fs::read(&ledger_path).unwrap();
        for event in [&target_delivery, &foreign_delivery] {
            serde_json::to_writer(&mut ledger_bytes, event).unwrap();
            ledger_bytes.push(b'\n');
        }
        fs::write(&ledger_path, &ledger_bytes).unwrap();
        fs::write(
            foreign_suffix.join("coordination/runtime/ledger-wal/r83.jsonl"),
            &ledger_bytes,
        )
        .unwrap();
        let main_before = storage_git(&foreign_suffix, &["rev-parse", "refs/heads/main"]);
        assert!(generic_delivery_storage_state(
            &foreign_suffix,
            "r83",
            &identity,
            &storage_events(&foreign_suffix),
        )
        .is_err());
        assert_eq!(
            storage_git(&foreign_suffix, &["rev-parse", "refs/heads/main"]),
            main_before
        );
        fs::remove_dir_all(foreign_suffix).unwrap();

        let (main_artifact_only, identity) = storage_fixture("storage-main-artifact-only");
        let artifact = storage_artifact_path(&main_artifact_only, &identity);
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"main orphan artifact\n").unwrap();
        storage_commit(&main_artifact_only, "commit artifact without event");
        assert!(generic_delivery_storage_state(
            &main_artifact_only,
            "r83",
            &identity,
            &storage_events(&main_artifact_only),
        )
        .is_err());
        fs::remove_dir_all(main_artifact_only).unwrap();

        let (main_event_only, identity) = storage_fixture("storage-main-event-only");
        crate::ledger::append(
            &main_event_only,
            "r83",
            &[storage_delivery(&identity)],
        )
        .unwrap();
        storage_commit(&main_event_only, "commit event without artifact");
        assert!(generic_delivery_storage_state(
            &main_event_only,
            "r83",
            &identity,
            &storage_events(&main_event_only),
        )
        .is_err());
        fs::remove_dir_all(main_event_only).unwrap();

        let (diverged, identity) = storage_fixture("storage-diverged-ledger");
        let replacement = crate::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"contractSchemaVersion": 3, "diverged": true}),
        );
        let mut bytes = serde_json::to_vec(&replacement).unwrap();
        bytes.push(b'\n');
        fs::write(
            diverged.join("coordination/rounds/r83/events.jsonl"),
            &bytes,
        )
        .unwrap();
        fs::write(
            diverged.join("coordination/runtime/ledger-wal/r83.jsonl"),
            &bytes,
        )
        .unwrap();
        assert!(generic_delivery_storage_state(
            &diverged,
            "r83",
            &identity,
            &storage_events(&diverged),
        )
        .is_err());
        fs::remove_dir_all(diverged).unwrap();

        let (wal_only, identity) = storage_fixture("storage-wal-only-drift");
        fs::write(
            wal_only.join("coordination/runtime/ledger-wal/r83.jsonl"),
            b"{\"not\":\"the ledger\"}\n",
        )
        .unwrap();
        assert!(generic_delivery_storage_state(
            &wal_only,
            "r83",
            &identity,
            &storage_events(&wal_only),
        )
        .is_err());
        fs::remove_dir_all(wal_only).unwrap();

        let (missing_wal_parent, identity) = storage_fixture("storage-missing-wal-parent");
        fs::remove_dir_all(missing_wal_parent.join("coordination/runtime/ledger-wal")).unwrap();
        assert!(generic_delivery_storage_state(
            &missing_wal_parent,
            "r83",
            &identity,
            &storage_events(&missing_wal_parent),
        )
        .is_err());
        assert!(!missing_wal_parent
            .join("coordination/runtime/ledger-wal")
            .exists());
        fs::remove_dir_all(missing_wal_parent).unwrap();

        let (ledger_only, identity) = storage_fixture("storage-ledger-only-drift");
        let mut ledger_bytes =
            fs::read(ledger_only.join("coordination/rounds/r83/events.jsonl")).unwrap();
        let extra = crate::ledger::event(
            "EscalationRaised",
            "runtime:orch",
            None,
            Some("r83"),
            serde_json::json!({"reason": "ledger-only fixture"}),
        );
        serde_json::to_writer(&mut ledger_bytes, &extra).unwrap();
        ledger_bytes.push(b'\n');
        fs::write(
            ledger_only.join("coordination/rounds/r83/events.jsonl"),
            &ledger_bytes,
        )
        .unwrap();
        assert!(generic_delivery_storage_state(
            &ledger_only,
            "r83",
            &identity,
            &storage_events(&ledger_only),
        )
        .is_err());
        fs::remove_dir_all(ledger_only).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let (bad_mode, identity) = storage_fixture("storage-bad-mode");
            let artifact = storage_artifact_path(&bad_mode, &identity);
            fs::create_dir_all(artifact.parent().unwrap()).unwrap();
            symlink("exact artifact\n", &artifact).unwrap();
            storage_commit(&bad_mode, "commit symlink artifact");
            fs::remove_file(&artifact).unwrap();
            fs::write(&artifact, b"exact artifact\n").unwrap();
            assert!(generic_delivery_storage_state(
                &bad_mode,
                "r83",
                &identity,
                &storage_events(&bad_mode),
            )
            .is_err());
            fs::remove_dir_all(bad_mode).unwrap();
        }
    }

    #[test]
    fn live_identity_rejects_zero_attempt_and_unsafe_path_components() {
        assert!(GenericReviewLiveIdentityV1::new("B321", "B321-A0000", "alpha", "wake-1").is_err());
        assert!(
            GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "../alpha", "wake-1").is_err()
        );
        assert!(GenericReviewLiveIdentityV1::new("B321", "B321-A0001", "alpha", "wake/1").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_install_is_no_clobber_single_link_and_parent_no_symlink() {
        use std::os::unix::fs::symlink;

        let root = generic_test_root("artifact");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("coordination/rounds/r83/reviews/review.md");
        install_new_canonical(&root, &artifact, b"exact\n").unwrap();
        assert!(install_new_canonical(&root, &artifact, b"exact\n").is_err());
        assert!(install_new_canonical(&root, &artifact, b"drift\n").is_err());
        let alias = root.join("artifact-hardlink");
        fs::hard_link(&artifact, &alias).unwrap();
        assert!(ensure_single_link(&artifact, "hardlinked generic review").is_err());
        fs::remove_dir_all(&root).unwrap();

        let root = generic_test_root("symlink-parent");
        let real = root.join("real-parent");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, root.join("coordination")).unwrap();
        assert!(install_new_canonical(
            &root,
            &root.join("coordination/rounds/r83/reviews/review.md"),
            b"exact\n"
        )
        .is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    struct ProjectionMismatchFixture {
        identity: GenericReviewIdentity,
        events: Vec<orch_core::EventRecord>,
    }

    fn projection_mismatch_fixture() -> ProjectionMismatchFixture {
        let identity = GenericReviewIdentity {
            task: "B320".to_string(),
            attempt: "B320-A0001".to_string(),
            harness: "smartclaw".to_string(),
            wake_id: B320_A0001_SMART_WAKE_ID.to_string(),
        };
        let head = "2a4302d2bf8b2f9a671b1961a6d964fb7c3d1300";
        let digest_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let digest_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let digest_c = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
        let digest_d = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let digest_e = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let binding = serde_json::json!({
            "attachmentManifestSha256": digest_a,
            "commandDigest": digest_b,
            "configDigest": digest_c,
            "cwdSelection": "project-root",
            "driver": "smartclaw",
            "effectiveTuple": {"provider": null, "model": null, "effort": null, "mode": null},
            "executableIdentityDigest": digest_d,
            "fixedHead": head,
            "harness": "smartclaw",
            "invocationCwd": "/repo",
            "observationSource": "dewusmartclaw-multica-session",
            "requestDigest": digest_e,
            "requestedTuple": {"provider": null, "model": null, "effort": null, "mode": null}
        });
        let output_path =
            "/repo/coordination/runtime/review-inbox/r83/B320-A0001-review-smartclaw.md";
        let mut lease = crate::ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "siteId": "B320-review-smartclaw-g01",
                "generation": 1,
                "attemptId": "B320-A0001",
                "role": "review",
                "agent": "smartclaw",
                "wakeId": B320_A0001_SMART_WAKE_ID,
                "reviewedHead": head,
                "paths": {"worktree": ".worktrees/review", "target": "orch/target/review"}
            }),
        );
        lease.event_id = B320_A0001_SMART_LEASE_EVENT_ID.to_string();
        let mut wake = crate::ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "action": "review",
                "method": "unified-channel-v1",
                "attemptId": "B320-A0001",
                "agent": "smartclaw",
                "harness": "smartclaw",
                "wakeId": B320_A0001_SMART_WAKE_ID,
                "providerKind": "smartclaw",
                "requestSessionId": format!("orch-wake-{}", B320_A0001_SMART_WAKE_ID),
                "attachmentManifestSha256": digest_a,
                "commandDigest": digest_b,
                "configDigest": digest_c,
                "cwdSelection": "project-root",
                "driver": "smartclaw",
                "effectiveTuple": {"provider": null, "model": null, "effort": null, "mode": null},
                "executableIdentityDigest": digest_d,
                "fixedHead": head,
                "invocationCwd": "/repo",
                "observationSource": "dewusmartclaw-multica-session",
                "requestDigest": digest_e,
                "requestedTuple": {"provider": null, "model": null, "effort": null, "mode": null}
            }),
        );
        wake.event_id = B320_A0001_SMART_WAKE_EVENT_ID.to_string();
        let mut request_payload = binding.clone();
        request_payload.as_object_mut().unwrap().extend(
            serde_json::json!({
                "attemptId": "B320-A0001",
                "role": "review",
                "agent": "smartclaw",
                "harness": "smartclaw",
                "wakeId": B320_A0001_SMART_WAKE_ID,
                "reviewedHead": head,
                "reviewOutputPath": output_path
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let mut request = crate::ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            request_payload,
        );
        request.event_id = B320_A0001_SMART_REQUEST_EVENT_ID.to_string();
        let mut receipt = crate::ledger::event(
            "AgentEventReceived",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "agentEvent": "wake-backend-receipt",
                "backendState": "accepted",
                "attemptId": "B320-A0001",
                "agent": "smartclaw",
                "actionId": B320_A0001_SMART_WAKE_ID,
                "wakeId": B320_A0001_SMART_WAKE_ID,
                "providerKind": "smartclaw",
                "receiptKind": "smartclaw",
                "requestSessionId": format!("orch-wake-{}", B320_A0001_SMART_WAKE_ID),
                "observedSessionId": format!("orch-wake-{}", B320_A0001_SMART_WAKE_ID),
                "channelBinding": binding,
                "probeEnd": 7,
            }),
        );
        receipt.event_id = B320_A0001_SMART_RECEIPT_EVENT_ID.to_string();
        let mut terminal = crate::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "agent": "smartclaw",
                "wakeId": B320_A0001_SMART_WAKE_ID,
                "channelBinding": binding,
                "state": "answered",
                "completionReason": "natural-exit",
                "outcomeClass": "TruncatedNoTerminal",
                "terminalSeen": false,
                "turnEnded": true,
                "exitedNaturally": true,
                "hardDeadlineReached": false,
                "managedScopeTerminated": true,
                "mechanicalTerminalAbsent": false,
                "cancelRequestId": null,
                "signals": [],
                "exactReason": "payloads-terminal",
                "logBytesRead": 7,
                "outputPath": output_path,
                "outputSha256": digest_b,
                "finalTextSha256": digest_c,
            }),
        );
        terminal.event_id = B320_A0001_SMART_TERMINAL_EVENT_ID.to_string();
        let mut release = crate::ledger::event(
            "WorkspaceReleased",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "siteId": "B320-review-smartclaw-g01",
                "generation": 1,
                "attemptId": "B320-A0001",
                "role": "review",
                "agent": "smartclaw",
                "wakeId": B320_A0001_SMART_WAKE_ID,
                "completionReceipt": "runtime:orch/managed-wake-terminated",
                "terminationEventId": terminal.event_id,
            }),
        );
        release.event_id = B320_A0001_SMART_RELEASE_EVENT_ID.to_string();
        let archived_root = crate::ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("B320"),
            Some("r83"),
            serde_json::json!({
                "attemptId": "B320-A0001", "verdict": "BLOCKED", "headSha": head
            }),
        );
        let recorded = crate::ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B320"),
            Some("r83"),
            serde_json::json!({"postMergeGates": "all-green"}),
        );
        ProjectionMismatchFixture {
            identity,
            events: vec![
                lease,
                wake,
                request,
                receipt,
                terminal,
                release,
                archived_root,
                recorded,
            ],
        }
    }

    fn recovered(fixture: &ProjectionMismatchFixture, verdict: crate::verify::RootVerdict) -> bool {
        bootstrap_projection_mismatch_closed_for_nonpass(
            &fixture.events,
            "r83",
            &fixture.identity,
            &fixture.events[2],
            &fixture.events[1],
            &fixture.events[0],
            &fixture.events[4],
            Some(&fixture.events[3]),
            verdict,
        )
        .unwrap()
    }

    #[test]
    fn b320_a0001_projection_mismatch_is_closed_only_for_nonpass() {
        let fixture = projection_mismatch_fixture();
        assert!(recovered(&fixture, crate::verify::RootVerdict::Fail));
        assert!(recovered(&fixture, crate::verify::RootVerdict::Blocked));
        assert!(!recovered(&fixture, crate::verify::RootVerdict::Pass));
        assert!(historical_b320_capacity_closed_v1(
            &fixture.events,
            "r83",
            "B320",
            "B320-A0001",
            "smartclaw",
            B320_A0001_SMART_WAKE_ID,
        )
        .unwrap());
    }

    #[test]
    fn b320_a0001_projection_mismatch_requires_accepted_receipt_and_exact_binding() {
        let mut fixture = projection_mismatch_fixture();
        assert!(!bootstrap_projection_mismatch_closed_for_nonpass(
            &fixture.events,
            "r83",
            &fixture.identity,
            &fixture.events[2],
            &fixture.events[1],
            &fixture.events[0],
            &fixture.events[4],
            None,
            crate::verify::RootVerdict::Fail,
        )
        .unwrap());
        fixture.events[3].payload.as_mut().unwrap()["channelBinding"]["requestDigest"] =
            serde_json::json!("dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd");
        assert!(!recovered(&fixture, crate::verify::RootVerdict::Fail));

        let mut fixture = projection_mismatch_fixture();
        fixture.events[2].payload.as_mut().unwrap()["configDigest"] =
            serde_json::json!("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        assert!(!recovered(&fixture, crate::verify::RootVerdict::Fail));
    }

    #[test]
    fn b320_a0001_projection_mismatch_rejects_unmanaged_release_drift_or_later_attempt() {
        let mut fixture = projection_mismatch_fixture();
        fixture.events[4].payload.as_mut().unwrap()["managedScopeTerminated"] =
            serde_json::json!(false);
        assert!(!recovered(&fixture, crate::verify::RootVerdict::Blocked));

        let mut fixture = projection_mismatch_fixture();
        fixture.events[5].payload.as_mut().unwrap()["terminationEventId"] =
            serde_json::json!("wrong-terminal");
        assert!(!recovered(&fixture, crate::verify::RootVerdict::Blocked));

        let mut fixture = projection_mismatch_fixture();
        fixture.identity.attempt = "B320-A0002".to_string();
        assert!(!recovered(&fixture, crate::verify::RootVerdict::Blocked));
    }
}

// Review identity admission moved from the retired scheduler without policy changes.
mod admission {
    use orch_core::EventRecord;
    /// Historical delivery projection key. Canonical schema-3 validation also
    /// binds the wake identity; this key cannot substitute for that binding.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct DeliveredReviewKey {
        /// Task owning the requested review.
        pub task_id: String,
        /// Exact attempt owning this delivery slot.
        pub attempt_id: String,
        /// Recorded wire role, without inferred seat policy.
        pub role: String,
        /// Exact recorded reviewer alias.
        pub agent: String,
    }

    fn event_payload_string<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
        event
            .payload
            .as_ref()
            .and_then(|payload| payload.get(key))
            .and_then(serde_json::Value::as_str)
    }

    fn required_task_id(event: &EventRecord) -> Result<String, String> {
        event
            .task_id
            .as_ref()
            .filter(|task| !task.trim().is_empty())
            .cloned()
            .ok_or_else(|| format!("{} 缺 taskId，无法判定在飞负载", event.kind))
    }

    fn required_payload_string(event: &EventRecord, key: &str) -> Result<String, String> {
        event_payload_string(event, key)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("{} 缺 payload.{key}，无法判定在飞负载", event.kind))
    }

    /// Exact event identity shared by review admission and the load projection.
    pub(crate) type GenericReviewCapacityKey = (String, String, String, String);

    /// Decode a generic review identity without deriving a policy seat.
    pub(crate) fn generic_review_capacity_key(
        event: &EventRecord,
    ) -> Result<Option<GenericReviewCapacityKey>, String> {
        let generic = match event.kind.as_str() {
            "ReviewRequested" | "WorkspaceLeased" => {
                event_payload_string(event, "role") == Some("review")
            }
            "WakeIssued" => event_payload_string(event, "action") == Some("review"),
            _ => false,
        };
        if !generic {
            return Ok(None);
        }
        let task = required_task_id(event)?;
        let attempt = required_payload_string(event, "attemptId")?;
        let agent = required_payload_string(event, "agent")?;
        let harness = match event.kind.as_str() {
            "WorkspaceLeased" => agent.clone(),
            _ => required_payload_string(event, "harness")?,
        };
        if harness != agent {
            return Err(format!("{} generic review agent/harness drift", event.kind));
        }
        let wake_id = required_payload_string(event, "wakeId")?;
        Ok(Some((task, attempt, harness, wake_id)))
    }

    /// Reject a schema-3 generic review identity before any lease or provider spawn.
    ///
    /// The mechanical key is exactly `(task, attempt, harness, wakeId)`. A harness
    /// can be requested at most once for one attempt, and a wake identity can never
    /// be reused by another action. This check deliberately knows nothing about
    /// reviewer rosters, providers, verdicts, vote counts, or quorum policy.
    pub(crate) fn validate_generic_review_admission_v1(
        events: &[EventRecord],
        round: &str,
        task: &str,
        attempt: &str,
        harness: &str,
        wake_id: &str,
    ) -> Result<(), String> {
        for (label, value) in [
            ("round", round),
            ("task", task),
            ("attempt", attempt),
            ("harness", harness),
            ("wakeId", wake_id),
        ] {
            if value.trim().is_empty() || value.trim() != value {
                return Err(format!("generic review admission {label} 非 canonical"));
            }
        }
        for event in events.iter().filter(|event| same_round(event, round)) {
            if event.kind == "WorkspaceLeased"
                && crate::sites::lease_is_pre_spawn_rejected(events, event)
            {
                continue;
            }
            if event_payload_string(event, "wakeId") == Some(wake_id)
                || (event.kind == "ActionRejected"
                    && event_payload_string(event, "actionId") == Some(wake_id))
            {
                return Err("generic review wakeId 已被 durable action 使用".to_string());
            }
            let Some((existing_task, existing_attempt, existing_harness, _)) =
                generic_review_capacity_key(event)?
            else {
                continue;
            };
            if existing_task == task && existing_attempt == attempt && existing_harness == harness {
                return Err("同 attempt 的 generic review harness 已被使用".to_string());
            }
        }
        Ok(())
    }

    fn same_round(event: &EventRecord, round: &str) -> bool {
        event
            .round
            .as_deref()
            .is_none_or(|event_round| event_round == round)
    }
}
/// Historical delivery projection key; live canonical validation additionally binds wakeId.
pub use admission::DeliveredReviewKey;
pub(crate) use admission::{
    generic_review_capacity_key, validate_generic_review_admission_v1, GenericReviewCapacityKey,
};
