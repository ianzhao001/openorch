//! Harness channel identities, capability descriptors, and admission checks.
//!
//! The registry is deliberately fail-closed: its bytes must parse without
//! unknown enum values, and its agent keys must exactly match the agent
//! registry before any descriptor can be used for process admission.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

#[cfg(feature = "selfhost")]
use crate::registry;

/// Repository-relative location of the harness capability registry.
pub const HARNESS_REGISTRY_RELPATH: &str = "coordination/harnesses.yaml";

/// Closed provider-neutral environment contract passed from the runtime to a managed wrapper.
pub const ENVELOPE_KEYS: &[&str] = &[
    "ORCH_HARNESS_ID",
    "ORCH_HARNESS_ACTION_ID",
    "ORCH_HARNESS_WAKE_ID",
    "ORCH_HARNESS_ROUND",
    "ORCH_HARNESS_TASK_ID",
    "ORCH_HARNESS_ATTEMPT_ID",
    "ORCH_HARNESS_ROLE",
    "ORCH_HARNESS_CWD",
    "ORCH_HARNESS_FIXED_HEAD",
    "ORCH_HARNESS_PROVIDER",
    "ORCH_HARNESS_MODEL",
    "ORCH_HARNESS_EFFORT",
    "ORCH_HARNESS_PROVIDER_BIN",
    "ORCH_HARNESS_REVIEW_OUTPUT_PATH",
    "ORCH_HARNESS_ORCH_BIN",
    "ORCH_HARNESS_DEADLINE_SECS",
];

/// Explicit review-output sentinel used by implementation invocations that cannot produce a review.
pub const NO_REVIEW_OUTPUT: &str = "NO_REVIEW_OUTPUT";

/// Validated invocation facts that a managed wrapper may trust before it starts a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationEnvelope {
    /// Canonical harness identity selected by the registered capability descriptor.
    pub harness_id: HarnessId,
    /// Durable runtime action whose side effect this invocation performs.
    pub action_id: String,
    /// Unique wake identity shared with the durable `WakeIssued` fact.
    pub wake_id: String,
    /// Active collaboration round that owns the invocation.
    pub round: String,
    /// Task identity carried into the provider process.
    pub task_id: String,
    /// Full attempt identity, including its task prefix and four-digit attempt number.
    pub attempt_id: String,
    /// Authorized invocation role (`review`, `implement`, or `consult`; legacy formal roles remain readable).
    pub role: String,
    /// Absolute execution or review-site working directory.
    pub cwd: PathBuf,
    /// Immutable lowercase forty-hex Git head bound to this invocation.
    pub fixed_head: String,
    /// Requested provider pin delivered to or verified by the wrapper.
    pub provider: String,
    /// Requested model pin delivered to or verified by the wrapper.
    pub model: String,
    /// Requested reasoning-effort pin delivered to or verified by the wrapper.
    pub effort: String,
    /// Absolute executable provider entry selected before wrapper launch.
    pub provider_bin: PathBuf,
    /// Absolute review inbox path, or `None` only for an implementation invocation.
    pub review_output: Option<PathBuf>,
    /// Absolute executable orch binary made available to the reviewer.
    pub orch_bin: PathBuf,
    /// Positive wall-clock ceiling applied to this provider invocation.
    pub deadline_secs: u64,
}

fn executable_envelope_path(key: &str, value: String) -> Result<PathBuf> {
    let path = PathBuf::from(&value);
    if !path.is_absolute() {
        bail!("{key} must be an absolute executable path: {value:?}");
    }
    let canonical = fs::canonicalize(&path).with_context(|| {
        format!(
            "{key} does not exist or cannot be resolved: {}",
            path.display()
        )
    })?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("{key} cannot be inspected: {}", canonical.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "{key} must name an executable regular file: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

impl InvocationEnvelope {
    /// Parse the closed map and refuse missing, blank, relative, malformed, or unexecutable facts.
    pub fn from_map(mut map: BTreeMap<String, String>) -> Result<Self> {
        let expected = ENVELOPE_KEYS.iter().copied().collect::<BTreeSet<_>>();
        let actual = map.keys().map(String::as_str).collect::<BTreeSet<_>>();
        if let Some(missing) = expected.difference(&actual).next() {
            bail!("invocation envelope missing required key {missing}");
        }
        if let Some(extra) = actual.difference(&expected).next() {
            bail!("invocation envelope contains unknown key {extra}");
        }

        let mut take = |key: &str| -> Result<String> {
            let value = map
                .remove(key)
                .with_context(|| format!("invocation envelope missing required key {key}"))?;
            if value.trim().is_empty() || value.trim() != value {
                bail!("invocation envelope {key} must be a non-blank exact string");
            }
            Ok(value)
        };

        let harness_text = take("ORCH_HARNESS_ID")?;
        let harness_id = HarnessId::parse(&harness_text)
            .map_err(|error| anyhow::anyhow!("ORCH_HARNESS_ID is invalid: {error}"))?;
        let action_id = take("ORCH_HARNESS_ACTION_ID")?;
        let wake_id = take("ORCH_HARNESS_WAKE_ID")?;
        let round = take("ORCH_HARNESS_ROUND")?;
        let task_id = take("ORCH_HARNESS_TASK_ID")?;
        let attempt_id = take("ORCH_HARNESS_ATTEMPT_ID")?;
        let expected_attempt_prefix = format!("{task_id}-A");
        let attempt_suffix = attempt_id.strip_prefix(&expected_attempt_prefix);
        if attempt_suffix.is_none_or(|suffix| {
            suffix.len() != 4 || !suffix.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            bail!("ORCH_HARNESS_ATTEMPT_ID must be the full {task_id}-A0000-shaped identity");
        }
        let role = take("ORCH_HARNESS_ROLE")?;
        if !matches!(
            role.as_str(),
            "primary" | "secondary" | "nongate" | "review" | "implement" | "consult"
        ) {
            bail!("ORCH_HARNESS_ROLE has an unsupported value: {role:?}");
        }
        let cwd_text = take("ORCH_HARNESS_CWD")?;
        let cwd = PathBuf::from(&cwd_text);
        if !cwd.is_absolute() {
            bail!("ORCH_HARNESS_CWD must be absolute: {cwd_text:?}");
        }
        let fixed_head = take("ORCH_HARNESS_FIXED_HEAD")?;
        if fixed_head.len() != 40
            || !fixed_head
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("ORCH_HARNESS_FIXED_HEAD must be forty lowercase hexadecimal characters");
        }
        let provider = take("ORCH_HARNESS_PROVIDER")?;
        let model = take("ORCH_HARNESS_MODEL")?;
        let effort = take("ORCH_HARNESS_EFFORT")?;
        let provider_bin = executable_envelope_path(
            "ORCH_HARNESS_PROVIDER_BIN",
            take("ORCH_HARNESS_PROVIDER_BIN")?,
        )?;
        let review_output_text = take("ORCH_HARNESS_REVIEW_OUTPUT_PATH")?;
        let review_output = if review_output_text == NO_REVIEW_OUTPUT {
            if !matches!(role.as_str(), "implement" | "consult") {
                bail!(
                    "ORCH_HARNESS_REVIEW_OUTPUT_PATH sentinel is valid only for non-review roles"
                );
            }
            None
        } else {
            let path = PathBuf::from(&review_output_text);
            if matches!(role.as_str(), "implement" | "consult") || !path.is_absolute() {
                bail!(
                    "ORCH_HARNESS_REVIEW_OUTPUT_PATH must be absolute for review roles and sentinel for implement"
                );
            }
            Some(path)
        };
        let orch_bin =
            executable_envelope_path("ORCH_HARNESS_ORCH_BIN", take("ORCH_HARNESS_ORCH_BIN")?)?;
        let deadline_text = take("ORCH_HARNESS_DEADLINE_SECS")?;
        let deadline_secs = deadline_text
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .with_context(|| {
                format!("ORCH_HARNESS_DEADLINE_SECS must be a positive integer: {deadline_text:?}")
            })?;

        Ok(Self {
            harness_id,
            action_id,
            wake_id,
            round,
            task_id,
            attempt_id,
            role,
            cwd,
            fixed_head,
            provider,
            model,
            effort,
            provider_bin,
            review_output,
            orch_bin,
            deadline_secs,
        })
    }

    /// Return the absolute review inbox for review roles while decoding the implementation sentinel as `None`.
    pub fn review_output_path(&self) -> Option<&Path> {
        self.review_output.as_deref()
    }
}

/// Canonical identity shared by wake providers and built-in consult adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HarnessId {
    /// OpenCode's direct CLI channel and consult adapter.
    OpenCode,
    /// Codex's direct CLI channel and consult adapter.
    Codex,
    /// Claude's CLI-backed review and consult channel.
    Claude,
    /// Cursor's built-in consult adapter identity.
    Cursor,
    /// MiMo's built-in consult adapter identity.
    Mimo,
    /// CodeBuddy's built-in consult adapter identity.
    CodeBuddy,
    /// SmartClaw's socket-injected channel.
    SmartClaw,
    /// Dclaw's HTTP-injected channel.
    Dclaw,
    /// Pi's managed streaming wrapper channel.
    Pi,
    /// ZCode's externally pinned streaming wrapper channel.
    ZCode,
    /// DSH's daemon-backed streaming wrapper channel.
    Dsh,
    /// Antigravity's direct CLI channel.
    Agy,
}

impl HarnessId {
    /// Complete closed vocabulary, ordered to preserve the legacy built-in adapter order.
    pub const ALL: &'static [HarnessId] = &[
        Self::OpenCode,
        Self::Codex,
        Self::Claude,
        Self::Cursor,
        Self::Mimo,
        Self::CodeBuddy,
        Self::SmartClaw,
        Self::Dclaw,
        Self::Pi,
        Self::ZCode,
        Self::Dsh,
        Self::Agy,
    ];

    /// Return the stable wire spelling used by existing logs and snapshots.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Cursor => "cursor",
            Self::Mimo => "mimo",
            Self::CodeBuddy => "codebuddy",
            Self::SmartClaw => "smartclaw",
            Self::Dclaw => "dclaw",
            Self::Pi => "pi",
            Self::ZCode => "zcode",
            Self::Dsh => "dsh",
            Self::Agy => "agy",
        }
    }

    /// Parse one exact wire spelling without aliases or case folding.
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == value)
            .ok_or_else(|| format!("unknown harness {value:?}"))
    }

    /// Identify the six identities historically exposed as built-in consult adapters.
    pub fn is_consult_builtin(&self) -> bool {
        matches!(
            self,
            Self::OpenCode
                | Self::Codex
                | Self::Claude
                | Self::Cursor
                | Self::Mimo
                | Self::CodeBuddy
        )
    }
}

impl Serialize for HarnessId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HarnessId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Process topology used to deliver one harness invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessTransport {
    /// A directly spawned command-line process.
    CliDirect,
    /// A wrapper that projects a command-line stream.
    CliStream,
    /// A daemon reached through an HTTP request.
    DaemonHttp,
    /// A long-lived client reached through a local socket.
    SocketInject,
    /// A long-lived client reached through an HTTP injection endpoint.
    HttpInject,
}

/// Location from which the provider's effective model selection originates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinSurface {
    /// The launch argument vector carries the effective selection.
    Argv,
    /// Environment variables carry the effective selection.
    Env,
    /// An external configuration file owns the effective selection.
    ExternalConfig,
    /// A user-controlled client-wide setting owns the effective selection.
    ClientGlobal,
}

/// Mechanism by which a signed provider/model/effort pin reaches the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinTransport {
    /// The signed value is already present literally in launch arguments.
    ArgvLiteral,
    /// The runtime passes the signed value through environment variables.
    Env,
    /// Environment variables verify, but do not replace, external configuration.
    EnvVerify,
    /// No mechanical route can carry a signed value to the provider process.
    None,
}

impl PinTransport {
    /// Report whether this transport can faithfully carry or verify a signed pin.
    pub fn carries_pin(&self) -> bool {
        !matches!(self, Self::None)
    }

    #[cfg(feature = "selfhost")]
    fn as_str(&self) -> &'static str {
        match self {
            Self::ArgvLiteral => "argv-literal",
            Self::Env => "env",
            Self::EnvVerify => "env-verify",
            Self::None => "none",
        }
    }
}

/// Provenance strength declared for receipts, terminal facts, or activity facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilitySource {
    /// The provider emits the fact in its native protocol.
    Native,
    /// A managed wrapper derives the fact from stricter lower-level evidence.
    Derived,
    /// The channel exposes no mechanical form of this fact.
    Absent,
}

impl CapabilitySource {
    /// Return the exact descriptor spelling used in durable wake facts.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Derived => "derived",
            Self::Absent => "absent",
        }
    }
}

/// Closed provider-neutral outcome vocabulary for one harness invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalState {
    /// A stable final answer or output artifact was observed after the turn ended.
    #[serde(rename = "answered")]
    Answered,
    /// The provider or wrapper ended with an explicit operational failure.
    #[serde(rename = "failed")]
    Failed,
    /// The authenticated invocation deadline elapsed before a final answer arrived.
    #[serde(rename = "timedOut")]
    TimedOut,
    /// The invocation ended without a stable final answer, including zero-frame EOF.
    #[serde(rename = "empty")]
    Empty,
    /// An authenticated cancellation stopped the invocation.
    #[serde(rename = "canceled")]
    Canceled,
}

impl TerminalState {
    /// Complete five-state vocabulary in stable declaration order.
    pub const ALL: &'static [TerminalState] = &[
        Self::Answered,
        Self::Failed,
        Self::TimedOut,
        Self::Empty,
        Self::Canceled,
    ];

    /// Return the exact wire spelling used in terminal records and manifests.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::TimedOut => "timedOut",
            Self::Empty => "empty",
            Self::Canceled => "canceled",
        }
    }

    /// Parse only an exact member of the closed vocabulary and reject every other value.
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "answered" => Ok(Self::Answered),
            "failed" => Ok(Self::Failed),
            "timedOut" => Ok(Self::TimedOut),
            "empty" => Ok(Self::Empty),
            "canceled" => Ok(Self::Canceled),
            other => Err(format!("unknown terminal state {other:?}")),
        }
    }
}

/// One wrapper-reserved exit code and its provider-neutral terminal meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapperExitCode {
    /// Process exit code reserved by one or more managed stream wrappers.
    pub code: i32,
    /// Exact [`TerminalState`] wire spelling associated with the reserved code.
    pub state: &'static str,
    /// Stable reason category; provider-native detail remains in `exactReason`.
    pub reason: &'static str,
}

/// Single Rust-side table shared by wake reconciliation and wrapper manifests.
pub const WRAPPER_EXIT_CODES: &[WrapperExitCode] = &[
    WrapperExitCode {
        code: 0,
        state: "answered",
        reason: "exact-terminal",
    },
    WrapperExitCode {
        code: 3,
        state: "failed",
        reason: "launch-failure",
    },
    WrapperExitCode {
        code: 64,
        state: "failed",
        reason: "invalid-arguments",
    },
    WrapperExitCode {
        code: 65,
        state: "failed",
        reason: "invalid-workspace",
    },
    WrapperExitCode {
        code: 66,
        state: "failed",
        reason: "invalid-envelope",
    },
    WrapperExitCode {
        code: 67,
        state: "failed",
        reason: "pin-mismatch",
    },
    WrapperExitCode {
        code: 70,
        state: "empty",
        reason: "zero-frame-eof",
    },
    WrapperExitCode {
        code: 71,
        state: "empty",
        reason: "truncated-no-terminal",
    },
    WrapperExitCode {
        code: 72,
        state: "timedOut",
        reason: "hard-deadline",
    },
    WrapperExitCode {
        code: 74,
        state: "failed",
        reason: "identity-drift",
    },
];

/// Immutable provider and supervisor facts supplied to the terminal classifier.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalObservation {
    /// Declared provenance of this harness's terminal evidence.
    pub capability: CapabilitySource,
    /// Wrapper or provider exit status after final drain, when available.
    pub exit_code: Option<i32>,
    /// Unmodified provider or supervisor reason text retained for audit.
    pub exact_reason: String,
    /// Whether an exact turn-ending frame was authenticated.
    pub turn_ended: bool,
    /// Stable final answer text, when the channel exposes it directly.
    pub final_text: Option<String>,
    /// Provider-computed SHA-256 when policy forbids copying final text into the wake log.
    pub final_text_sha256: Option<String>,
    /// Stable output artifact path, when the invocation writes one.
    pub output_path: Option<String>,
    /// SHA-256 of the output artifact bytes paired with `output_path`.
    pub output_sha256: Option<String>,
    /// Provider-supplied usage object, without invented zero values.
    pub usage: Option<serde_json::Value>,
    /// Explicit explanation required whenever provider usage is absent.
    pub usage_absent_reason: Option<String>,
    /// Whether the runtime proved the managed process scope fully terminated.
    pub managed_scope_terminated: bool,
    /// Whether non-terminal activity was observed; activity never promotes a state.
    pub activity_seen: bool,
    /// Whether an authenticated cancellation request caused termination.
    pub authenticated_cancel: bool,
}

/// Durable provider-neutral terminal envelope emitted by wake reconciliation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalRecord {
    /// One exact member of the closed five-state terminal vocabulary.
    pub state: TerminalState,
    /// Provider or supervisor reason preserved without categorical rewriting.
    pub exact_reason: String,
    /// Whether the provider emitted an authenticated turn-ending frame.
    pub turn_ended: bool,
    /// SHA-256 of stable final answer text, or `null` when no answer exists.
    pub final_text_sha256: Option<String>,
    /// Stable output artifact path, or `null` when no artifact exists.
    pub output_path: Option<String>,
    /// SHA-256 paired with `outputPath`, or `null` when no artifact exists.
    pub output_sha256: Option<String>,
    /// Provider usage bytes as structured JSON, or explicit `null` when absent.
    pub usage: Option<serde_json::Value>,
    /// Non-empty reason paired with a `null` usage field.
    pub usage_absent_reason: Option<String>,
    /// Whether the runtime proved that its complete managed scope terminated.
    pub managed_scope_terminated: bool,
    /// Whether the descriptor explicitly declares that no mechanical terminal exists.
    pub mechanical_terminal_absent: bool,
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn wrapper_exit_code(code: i32) -> Option<&'static WrapperExitCode> {
    WRAPPER_EXIT_CODES.iter().find(|entry| entry.code == code)
}

/// EOF transport diagnostics may coexist with a separately proven native final.
/// Timeout, cancellation, launch and identity failures never match this predicate.
pub(crate) fn wrapper_exit_is_transport_eof(code: Option<i32>) -> bool {
    code.and_then(wrapper_exit_code).is_some_and(|entry|
        matches!(entry.reason, "zero-frame-eof" | "truncated-no-terminal"))
}

#[cfg(test)]
mod channel_exit_contract_tests {
    use super::*;

    #[test]
    fn direct_exit_codes_do_not_inherit_the_reserved_wrapper_abi() {
        let observation = |code| TerminalObservation {
            capability: CapabilitySource::Derived, exit_code: Some(code),
            exact_reason: "observed numeric exit".into(), turn_ended: false,
            final_text: None, final_text_sha256: None, output_path: None, output_sha256: None,
            usage: None, usage_absent_reason: None, managed_scope_terminated: true,
            activity_seen: false, authenticated_cancel: false,
        };
        for (code, wrapper_state) in [(72,TerminalState::TimedOut),(70,TerminalState::Empty),(71,TerminalState::Empty)] {
            assert_eq!(classify_terminal_observation(observation(code)).unwrap().state, wrapper_state,
                "legacy wrapper ABI must remain intact");
            assert_eq!(classify_terminal_observation_with_wrapper_codes(observation(code),true).unwrap().state, wrapper_state);
            let direct = classify_terminal_observation_with_wrapper_codes(observation(code),false).unwrap();
            assert_eq!(direct.state, TerminalState::Failed);
            assert!(!direct.turn_ended && direct.final_text_sha256.is_none());
        }
    }
}

/// Classify final-drained observations without treating exit zero, activity, or PID loss as an answer.
pub fn classify_terminal_observation(observation: TerminalObservation) -> Result<TerminalRecord> {
    classify_terminal_observation_with_wrapper_codes(observation, true)
}

/// A channel knows whether its code-owned driver used the reserved wrapper ABI.
/// Direct executables retain actual numeric status without inheriting wrapper meanings.
pub(crate) fn classify_terminal_observation_with_wrapper_codes(
    observation: TerminalObservation,
    wrapper_codes: bool,
) -> Result<TerminalRecord> {
    let exact_reason = observation.exact_reason;
    if exact_reason.trim().is_empty() || exact_reason.trim() != exact_reason {
        bail!("terminal exactReason must be a non-blank exact string");
    }

    if observation.capability == CapabilitySource::Absent {
        return Ok(TerminalRecord {
            state: TerminalState::Empty,
            exact_reason,
            turn_ended: false,
            final_text_sha256: None,
            output_path: None,
            output_sha256: None,
            usage: None,
            usage_absent_reason: Some(
                "harness descriptor declares no mechanical usage or terminal source".to_string(),
            ),
            managed_scope_terminated: false,
            mechanical_terminal_absent: true,
        });
    }

    let output_pair_present =
        observation.output_path.is_some() && observation.output_sha256.is_some();
    if observation.output_path.is_some() != observation.output_sha256.is_some() {
        bail!("terminal outputPath and outputSha256 must be present together");
    }
    if observation
        .output_sha256
        .as_deref()
        .is_some_and(|digest| !valid_sha256(digest))
    {
        bail!("terminal outputSha256 must be sixty-four lowercase hexadecimal characters");
    }
    if observation.final_text.is_some() && observation.final_text_sha256.is_some() {
        bail!("terminal finalText and finalTextSha256 are mutually exclusive");
    }
    if observation
        .final_text_sha256
        .as_deref()
        .is_some_and(|digest| !valid_sha256(digest))
    {
        bail!("terminal finalTextSha256 must be sixty-four lowercase hexadecimal characters");
    }
    let final_text_sha256 = observation
        .final_text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(|text| hex::encode(Sha256::digest(text.as_bytes())))
        .or(observation.final_text_sha256);
    if observation
        .final_text
        .as_deref()
        .is_some_and(|text| text.trim().is_empty())
    {
        bail!("terminal finalText must not be blank");
    }

    let stable_answer =
        observation.turn_ended && (final_text_sha256.is_some() || output_pair_present);
    // Activity is retained as an input so callers cannot silently omit that
    // observation, but it deliberately has no branch that promotes terminality.
    let _activity_seen = observation.activity_seen;
    let state = if observation.authenticated_cancel {
        TerminalState::Canceled
    } else if observation.exit_code == Some(0) && stable_answer {
        TerminalState::Answered
    } else if observation.exit_code == Some(0) {
        TerminalState::Empty
    } else if let Some(code) = observation.exit_code {
        wrapper_codes.then(|| wrapper_exit_code(code)).flatten()
            .map(|entry| TerminalState::parse(entry.state))
            .transpose()
            .map_err(anyhow::Error::msg)?
            .unwrap_or(TerminalState::Failed)
    } else if observation.turn_ended && stable_answer {
        TerminalState::Answered
    } else {
        TerminalState::Failed
    };

    let usage_absent_reason = match (&observation.usage, observation.usage_absent_reason) {
        (Some(_), Some(reason)) if !reason.trim().is_empty() => {
            bail!("terminal usageAbsentReason must be null when usage is present")
        }
        (Some(_), _) => None,
        (None, Some(reason)) if !reason.trim().is_empty() => Some(reason),
        (None, _) => Some("provider did not report usage".to_string()),
    };
    if state == TerminalState::Answered && !stable_answer {
        bail!("answered terminal requires an ended turn and stable final text or output artifact");
    }

    Ok(TerminalRecord {
        state,
        exact_reason,
        turn_ended: observation.turn_ended,
        final_text_sha256,
        output_path: observation.output_path,
        output_sha256: observation.output_sha256,
        usage: observation.usage,
        usage_absent_reason,
        managed_scope_terminated: observation.managed_scope_terminated,
        mechanical_terminal_absent: false,
    })
}

/// Per-operation control capabilities exposed by a harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlCapabilities {
    /// Whether exact wake status can be queried.
    pub status: bool,
    /// Whether an authenticated cancellation can be requested.
    pub cancel: bool,
    /// Whether a terminal review session can be attached.
    pub attach: bool,
    /// Whether a dead formal review wake can be reissued.
    pub reissue: bool,
    /// Whether immutable terminal evidence can declare a review wake dead.
    pub declare_dead: bool,
}

/// Closed action vocabulary used by the code-owned driver catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverAction {
    /// Direct implementation or execution.
    Execute,
    /// Fixed-head review.
    Review,
    /// Explicit-member consultation.
    Consult,
}

impl DriverAction {
    /// Return the stable action spelling used in diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Review => "review",
            Self::Consult => "consult",
        }
    }
}

/// Immutable mechanics owned by one driver/action pair.
///
/// Local configuration may select a driver and pin a tuple, but cannot replace
/// any transport, wrapper, receipt, terminal, activity, control, or observation
/// fact declared here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverContract {
    /// Process topology used for this action.
    pub transport: HarnessTransport,
    /// Repository-relative code-owned wrapper, when a stream wrapper is needed.
    pub wrapper: Option<&'static str>,
    /// Strength of the provider acceptance receipt.
    pub receipt: CapabilitySource,
    /// Strength of the unique terminal observation.
    pub terminal: CapabilitySource,
    /// Strength of non-terminal progress observations.
    pub activity: CapabilitySource,
    /// Supported control-plane operations.
    pub control: ControlCapabilities,
    /// Stable description of the execution-level observation source.
    pub observation_source: &'static str,
}

const NO_CONTROL: ControlCapabilities = ControlCapabilities {
    status: false,
    cancel: false,
    attach: false,
    reissue: false,
    declare_dead: false,
};

const STATUS_CANCEL_CONTROL: ControlCapabilities = ControlCapabilities {
    status: true,
    cancel: true,
    attach: false,
    reissue: false,
    declare_dead: false,
};



impl HarnessId {
    /// Report whether this driver implements the requested action at all.
    pub fn supports_action(self, action: DriverAction) -> bool {
        match self {
            Self::Cursor | Self::Mimo | Self::CodeBuddy => action == DriverAction::Consult,
            Self::SmartClaw => {
                matches!(action, DriverAction::Review | DriverAction::Consult)
            }
            Self::Dclaw => false,
            Self::Pi | Self::ZCode | Self::Dsh | Self::Agy => action != DriverAction::Consult,
            Self::OpenCode | Self::Codex | Self::Claude => true,
        }
    }

    /// Resolve the single code-owned mechanics record for one supported action.
    pub fn driver_contract(self, action: DriverAction) -> Option<DriverContract> {
        if !self.supports_action(action) {
            return None;
        }
        let contract = match self {
            Self::OpenCode => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Native,
                terminal: CapabilitySource::Native,
                activity: CapabilitySource::Native,
                control: STATUS_CANCEL_CONTROL,
                observation_source: "opencode-db-and-json-stream",
            },
            Self::Codex => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Native,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Native,
                control: STATUS_CANCEL_CONTROL,
                observation_source: "codex-rollout-turn-context-and-json-stream",
            },
            Self::Claude if action == DriverAction::Review => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Absent,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Native,
                control: NO_CONTROL,
                observation_source: "claude-stream-result",
            },
            Self::Claude => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Absent,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Native,
                control: NO_CONTROL,
                observation_source: "claude-stream-result",
            },
            Self::Cursor => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Absent,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Absent,
                control: NO_CONTROL,
                observation_source: "cursor-agent-final-output",
            },
            Self::Mimo => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Absent,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Absent,
                control: NO_CONTROL,
                observation_source: "mimo-final-output",
            },
            Self::CodeBuddy => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Absent,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Absent,
                control: NO_CONTROL,
                observation_source: "codebuddy-final-output",
            },
            Self::SmartClaw => DriverContract {
                transport: HarnessTransport::SocketInject,
                wrapper: Some("coordination/scripts/wake-multica.sh"),
                receipt: CapabilitySource::Native,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Derived,
                control: NO_CONTROL,
                observation_source: "dewusmartclaw-native-final-and-stream-v1",
            },
            Self::Dclaw => DriverContract {
                transport: HarnessTransport::HttpInject,
                wrapper: Some("coordination/scripts/wake-dclaw.sh"),
                receipt: CapabilitySource::Absent,
                terminal: CapabilitySource::Absent,
                activity: CapabilitySource::Absent,
                control: NO_CONTROL,
                observation_source: "none",
            },
            Self::Pi => DriverContract {
                transport: HarnessTransport::CliStream,
                wrapper: Some("orch/scripts/wake-pi-stream.sh"),
                receipt: CapabilitySource::Native,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Derived,
                control: STATUS_CANCEL_CONTROL,
                observation_source: "pi-session-jsonl-and-stream",
            },
            Self::ZCode => DriverContract {
                transport: HarnessTransport::CliStream,
                wrapper: Some("orch/scripts/wake-zcode-stream.sh"),
                receipt: CapabilitySource::Native,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Derived,
                control: STATUS_CANCEL_CONTROL,
                observation_source: "zcode-rollout-and-stream",
            },
            Self::Dsh => DriverContract {
                transport: HarnessTransport::CliStream,
                wrapper: Some("orch/scripts/wake-dsh-stream.sh"),
                receipt: CapabilitySource::Derived,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Derived,
                control: NO_CONTROL,
                observation_source: "dsh-session-transcript",
            },
            Self::Agy => DriverContract {
                transport: HarnessTransport::CliDirect,
                wrapper: None,
                receipt: CapabilitySource::Derived,
                terminal: CapabilitySource::Derived,
                activity: CapabilitySource::Native,
                control: NO_CONTROL,
                observation_source: "agy-conversation-db-and-cli-log",
            },
        };
        Some(contract)
    }
}

/// Machine-readable capabilities for one registered agent's channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HarnessDescriptor {
    /// Canonical channel identity used across provider vocabularies.
    pub harness: HarnessId,
    /// Process topology used to reach the channel.
    pub transport: HarnessTransport,
    /// Repository-relative wrapper path, or no wrapper for direct commands.
    pub wrapper: Option<String>,
    /// Source of the provider's effective model selection.
    pub pin_surface: PinSurface,
    /// Route by which signed pin values reach or verify the process.
    pub pin_transport: PinTransport,
    /// Provenance available for an action-scoped backend receipt.
    pub receipt: CapabilitySource,
    /// Provenance available for a unique mechanical terminal fact.
    pub terminal: CapabilitySource,
    /// Provenance available for structured progress activity.
    pub activity: CapabilitySource,
    /// Exact operations supported by the channel's control plane.
    pub control: ControlCapabilities,
    /// Human-readable location of execution-level model truth.
    pub model_truth: String,
    /// Operational caveats retained alongside the mechanical declaration.
    pub notes: String,
}

/// Validated descriptor table whose keys exactly equal the agent registry keys.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(feature = "selfhost")]
pub struct HarnessRegistry {
    descriptors: BTreeMap<String, HarnessDescriptor>,
}

#[cfg(feature = "selfhost")]
impl HarnessRegistry {
    /// Return every validated agent key in deterministic registry order.
    pub fn agent_ids(&self) -> Vec<String> {
        self.descriptors.keys().cloned().collect()
    }

    /// Look up the validated channel capabilities for one exact agent key.
    pub fn get(&self, agent: &str) -> Option<&HarnessDescriptor> {
        self.descriptors.get(agent)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[cfg(feature = "selfhost")]
struct HarnessRegistryFile {
    api_version: String,
    kind: String,
    #[serde(rename = "metadata")]
    _metadata: serde_yaml::Value,
    harnesses: BTreeMap<String, HarnessDescriptor>,
}

/// Load descriptors and reject malformed files or any drift from the agent key set.
#[cfg(feature = "selfhost")]
pub fn load_harness_registry(root: &Path) -> Result<HarnessRegistry> {
    let path = root.join(HARNESS_REGISTRY_RELPATH);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("读取 HarnessRegistry 失败: {}", path.display()))?;
    let parsed: HarnessRegistryFile = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 HarnessRegistry 失败: {}", path.display()))?;
    if parsed.api_version != "orch/v1alpha1" || parsed.kind != "HarnessRegistry" {
        bail!("HarnessRegistry 的 apiVersion/kind 不匹配");
    }

    let definitions =
        registry::load_agent_definitions(root).context("HarnessRegistry 无法对质 AgentRegistry")?;
    let registered: BTreeSet<String> = definitions.keys().cloned().collect();
    let described: BTreeSet<String> = parsed.harnesses.keys().cloned().collect();
    let missing: Vec<String> = registered.difference(&described).cloned().collect();
    let extra: Vec<String> = described.difference(&registered).cloned().collect();
    if !missing.is_empty() || !extra.is_empty() {
        bail!(
            "HarnessRegistry 与 AgentRegistry agent 键集合不一致: 缺少描述符={missing:?}; 多余描述符={extra:?}"
        );
    }

    Ok(HarnessRegistry {
        descriptors: parsed.harnesses,
    })
}

/// Hash the descriptor file's exact bytes without YAML normalization or trimming.
#[cfg(feature = "selfhost")]
pub fn registry_digest(root: &Path) -> Result<String> {
    let path = root.join(HARNESS_REGISTRY_RELPATH);
    let bytes = fs::read(&path)
        .with_context(|| format!("读取 HarnessRegistry 摘要输入失败: {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Refuse signed pins that the selected harness cannot carry to its process.
#[cfg(feature = "selfhost")]
pub fn assert_pin_is_transportable(
    registry: &HarnessRegistry,
    agent: &str,
    def: &registry::AgentDefinition,
) -> Result<()> {
    let descriptor = registry
        .get(agent)
        .with_context(|| format!("HarnessRegistry 缺少 agent {agent}"))?;
    let has_pin = def.provider.is_some() || def.model.is_some() || def.effort.is_some();
    if has_pin && !descriptor.pin_transport.carries_pin() {
        bail!(
            "agent {agent} harness {} 声明 signed pin，但 pinTransport={} 无法把 pin 送达进程",
            descriptor.harness.as_str(),
            descriptor.pin_transport.as_str()
        );
    }
    Ok(())
}
