//! Explicit-member lightweight consultation over the unified harness channel.
//!
//! Schema 3 keeps membership, models, effort and timeouts in the action-local
//! invocation snapshot.  Cards and ROUND-IR never acquire a consultation
//! roster, judge, seat, quorum or error-trigger policy.  Each member retains an
//! independent raw answer and manifest; the root summary is only an index.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;
#[cfg(feature = "selfhost")]
use orch_core::EventRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{adapter, failure::FailureClass, redact};
#[cfg(feature = "selfhost")]
use crate::plan;

const CONSULTATIONS_PATH: &str = "coordination/consultations";
const CHANNEL_V3_MEMBER_TIMEOUT_SECS: u64 = 900;
const CHANNEL_V3_TOTAL_WALL_SECS: u64 = 1800;
const CHANNEL_V3_MAX_MEMBERS: usize = 5;

/// Version of the explicit-member, policy-free consultation contract.
pub const LIGHTWEIGHT_CONSULT_CONTRACT_V1: u32 = 1;

/// Whether the current protocol state permits a plan-phase consultation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum GateDecision {
    Admit { basis: String },
    Refuse { reason: String },
}

/// Code-owned ceilings shared by explicit channel consult and legacy fusion tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsultLimits {
    pub per_member_timeout_secs: u64,
    pub total_wall_secs: u64,
    pub max_members: usize,
}

impl ConsultLimits {
    fn validate(self, context: &str) -> Result<Self> {
        if self.per_member_timeout_secs == 0 {
            bail!("{context} perMemberTimeoutSecs 必须大于 0");
        }
        if self.total_wall_secs == 0 {
            bail!("{context} totalWallSecs 必须大于 0");
        }
        if self.max_members == 0 {
            bail!("{context} maxMembers 必须大于 0");
        }
        Ok(self)
    }
}

/// One stable input slot in a consultation fusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FusionMember {
    pub index: usize,
    pub member: String,
}

impl FusionMember {
    pub fn new(index: usize, member: impl Into<String>) -> Self {
        Self {
            index,
            member: member.into(),
        }
    }
}

/// Member terminal state. Timeouts stay distinct from ordinary failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemberStatus {
    Ok,
    Failed,
    TimedOut,
}

/// How a fusion answer was obtained after provider output normalisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerExtraction {
    Structured,
    PlainText,
    RawTranscript,
}

impl AnswerExtraction {
    /// Only a suspicious transcript fallback should produce a CLI warning.
    pub fn is_loud(self) -> bool {
        matches!(self, Self::RawTranscript)
    }
}

/// A fusion answer after provider JSON-lines have been normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedAnswer {
    pub text: String,
    pub extraction: AnswerExtraction,
}

/// Stable metadata/CLI label for one extraction outcome.
pub fn answer_extraction_label(answer: &ExtractedAnswer) -> &'static str {
    match answer.extraction {
        AnswerExtraction::Structured => "structured",
        AnswerExtraction::PlainText => "plain-text",
        AnswerExtraction::RawTranscript => "raw-transcript",
    }
}

/// Ordered action-local harness aliases selected for one consultation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplicitConsultMembers {
    aliases: Vec<String>,
}

impl ExplicitConsultMembers {
    /// Validate a non-empty, duplicate-free list without rewriting aliases.
    pub fn new<I, S>(members: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let aliases = members.into_iter().map(Into::into).collect::<Vec<_>>();
        if aliases.is_empty() || aliases.len() > CHANNEL_V3_MAX_MEMBERS {
            bail!(
                "consult --harness 成员数必须在 1..={} 内",
                CHANNEL_V3_MAX_MEMBERS
            );
        }
        let mut seen = HashSet::new();
        for alias in &aliases {
            if alias.trim() != alias || !safe_harness_alias(alias) {
                bail!("consult harness alias 非安全原样 identity: {alias:?}");
            }
            if !seen.insert(alias.clone()) {
                bail!("consult harness alias 重复: {alias}");
            }
        }
        Ok(Self { aliases })
    }

    /// Return aliases in the exact caller-supplied order.
    pub fn as_slice(&self) -> &[String] {
        &self.aliases
    }
}

/// Immutable invocation and raw-artifact binding for one consult member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsultMemberManifestV3 {
    alias: String,
    fixed_head: String,
    cwd: PathBuf,
    request_digest: String,
    config_digest: String,
    ordered_attachment_manifest_digest: String,
    artifact_sha256: String,
    artifact_bytes: u64,
}

impl ConsultMemberManifestV3 {
    /// Construct one exact invocation/output binding; no field is inferred.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        alias: impl Into<String>,
        fixed_head: impl Into<String>,
        cwd: impl Into<PathBuf>,
        request_digest: impl Into<String>,
        config_digest: impl Into<String>,
        ordered_attachment_manifest_digest: impl Into<String>,
        artifact_sha256: impl Into<String>,
        artifact_bytes: u64,
    ) -> Result<Self> {
        let manifest = Self {
            alias: alias.into(),
            fixed_head: fixed_head.into(),
            cwd: cwd.into(),
            request_digest: request_digest.into(),
            config_digest: config_digest.into(),
            ordered_attachment_manifest_digest: ordered_attachment_manifest_digest.into(),
            artifact_sha256: artifact_sha256.into(),
            artifact_bytes,
        };
        if !safe_harness_alias(&manifest.alias)
            || !is_lower_hex(&manifest.fixed_head, 40)
            || !absolute_lexical_path(&manifest.cwd)
            || !is_lower_hex(&manifest.request_digest, 64)
            || !is_lower_hex(&manifest.config_digest, 64)
            || !is_lower_hex(&manifest.ordered_attachment_manifest_digest, 64)
            || !is_lower_hex(&manifest.artifact_sha256, 64)
        {
            bail!("consult member manifest 含非 canonical identity/digest/path");
        }
        Ok(manifest)
    }

    /// Match the fixed request snapshot without consulting live files.
    pub fn matches_invocation(
        &self,
        fixed_head: &str,
        cwd: &str,
        request_digest: &str,
        config_digest: &str,
        ordered_attachment_manifest_digest: &str,
    ) -> bool {
        self.fixed_head == fixed_head
            && self.cwd == Path::new(cwd)
            && self.request_digest == request_digest
            && self.config_digest == config_digest
            && self.ordered_attachment_manifest_digest == ordered_attachment_manifest_digest
    }

    /// Match the exact raw answer bytes captured for this member.
    pub fn matches_artifact(&self, artifact_sha256: &str, artifact_bytes: u64) -> bool {
        self.artifact_sha256 == artifact_sha256 && self.artifact_bytes == artifact_bytes
    }
}

/// Facts used to decide whether one captured member answer is substantive.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsultAnswerInputV3 {
    terminal: crate::harness::TerminalRecord,
    final_text: String,
    tool_only: bool,
    raw_transcript: bool,
}

impl ConsultAnswerInputV3 {
    /// Bind final text and extraction flags to one provider-neutral terminal.
    pub fn from_terminal(
        terminal: crate::harness::TerminalRecord,
        final_text: impl Into<String>,
        tool_only: bool,
        raw_transcript: bool,
    ) -> Self {
        Self {
            terminal,
            final_text: final_text.into(),
            tool_only,
            raw_transcript,
        }
    }
}

/// Policy-free validity of a single consultation answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsultAnswerValidityV3 {
    /// A trusted terminal and substantive, byte-bound final answer agree.
    Valid,
    /// The member is invalid for the retained mechanical reason.
    Invalid(String),
}

/// Classify answer truth without voting, vetoing, selecting a roster or synthesising.
pub fn classify_consult_answer_v3(input: &ConsultAnswerInputV3) -> ConsultAnswerValidityV3 {
    use crate::harness::TerminalState;

    let invalid = |reason: &str| ConsultAnswerValidityV3::Invalid(reason.to_string());
    if input.terminal.mechanical_terminal_absent {
        return invalid("terminal source is absent");
    }
    if input.tool_only {
        return invalid("answer is tool-only");
    }
    if input.raw_transcript {
        return invalid("answer is a raw transcript");
    }
    if input.terminal.state != TerminalState::Answered {
        return invalid("terminal is not answered");
    }
    if !input.terminal.turn_ended {
        return invalid("terminal turn did not end");
    }
    if !input.terminal.managed_scope_terminated {
        return invalid("managed scope is not terminated");
    }
    if input.final_text.trim().is_empty() {
        return invalid("final text is empty");
    }
    let digest = sha256(input.final_text.as_bytes());
    if input.terminal.final_text_sha256.as_deref() != Some(digest.as_str()) {
        return invalid("final text bytes do not match the trusted terminal digest");
    }
    ConsultAnswerValidityV3::Valid
}

/// Durable-data-neutral result shared by explicit channel consult and legacy fusion helpers.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberOutcome {
    pub index: usize,
    pub member: String,
    pub status: MemberStatus,
    pub answer: Option<String>,
    pub answer_extraction: Option<String>,
    pub failure_class: Option<FailureClass>,
    pub reason: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_secs: u64,
    pub usage: Option<serde_json::Value>,
    pub observed_model: Option<String>,
    pub same_tool_as_planner: bool,
    pub worktree: Option<PathBuf>,
    pub target_dir: Option<PathBuf>,
    /// Schema-3 channel request, snapshot, observation, receipt, and terminal facts.
    pub channel_facts: Option<serde_json::Value>,
}

impl MemberOutcome {
    pub fn new(index: usize, member: impl Into<String>, status: MemberStatus) -> Self {
        Self {
            index,
            member: member.into(),
            status,
            answer: None,
            answer_extraction: None,
            failure_class: None,
            reason: None,
            exit_code: None,
            duration_secs: 0,
            usage: None,
            observed_model: None,
            same_tool_as_planner: false,
            worktree: None,
            target_dir: None,
            channel_facts: None,
        }
    }
}

/// Inputs accepted by the explicit-member consultation driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultArgs {
    /// Repository-local UTF-8 question file.
    pub question: PathBuf,
    /// Exact ordered harness aliases supplied through repeated `--harness`.
    pub harnesses: Vec<String>,
    /// Ordered repository-local UTF-8 attachments.
    pub attachments: Vec<PathBuf>,
    /// Explicit member deadline; action config then the code-owned default
    /// applies when absent. The shared total-wall ceiling always bounds it.
    pub member_timeout_secs: Option<u64>,
    /// Optional wall ceiling shared by every concurrent member.
    pub total_wall_secs: Option<u64>,
}

impl Default for ConsultArgs {
    fn default() -> Self {
        Self {
            question: PathBuf::new(),
            harnesses: Vec::new(),
            attachments: Vec::new(),
            member_timeout_secs: None,
            total_wall_secs: None,
        }
    }
}

/// Durable location and terminal summary returned to the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsultOutcome {
    /// ULID-scoped consultation identity.
    pub id: String,
    /// Immutable consultation artifact directory.
    pub dir: PathBuf,
    /// One stable result slot for every explicit harness.
    pub members: Vec<MemberOutcome>,
    /// Root-authored index; it never replaces or synthesises member answers.
    pub summary_path: PathBuf,
}

/// Paths allocated for a successful consultation before any member is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultationSkeleton {
    pub id: String,
    pub dir: PathBuf,
    pub request_dir: PathBuf,
    pub fusion_dir: PathBuf,
}

/// Historical pure gate fold for schema-less rounds.
///
/// Schema 3 is handled by [`consultation_admitted`] and deliberately permits
/// consultation before and after matching plan sign-off.
#[cfg(feature = "selfhost")]
pub fn consultation_gate(events: &[EventRecord], round: &str) -> GateDecision {
    if round.is_empty() {
        return GateDecision::Refuse {
            reason: "consultation gate 无法求值：CURRENT-ROUND 为空".to_string(),
        };
    }

    let closed = events.iter().any(|event| {
        event.kind == "RoundClosed"
            && event.actor == "runtime:orch"
            && event.task_id.is_none()
            && event.round.as_deref() == Some(round)
    });
    if closed {
        return GateDecision::Admit {
            basis: format!("round={round} 已 RoundClosed，无在飞落地期（legacy pure fold）"),
        };
    }

    let latest = events
        .iter()
        .enumerate()
        .filter_map(|(position, event)| {
            plan::decode_runtime_task_validated(event, round)
                .ok()
                .map(|validation| (position, validation))
        })
        .last();
    let Some((validation_position, validation)) = latest else {
        return GateDecision::Admit {
            basis: format!("round={round} 尚无 canonical production TaskValidated"),
        };
    };

    match plan::matching_user_plan_signoff(
        &events[validation_position..],
        round,
        validation.ir_revision,
        &validation.validation_digest,
    ) {
        Ok(true) => GateDecision::Refuse {
            reason: format!(
                "PlanSignedOff freezes consultation: round={round} irRevision={} validationDigest={}…",
                validation.ir_revision,
                digest_prefix(&validation.validation_digest)
            ),
        },
        Ok(false) => GateDecision::Admit {
            basis: format!(
                "round={round} plan phase open: irRevision={} validationDigest={}… 未获匹配 PlanSignedOff",
                validation.ir_revision,
                digest_prefix(&validation.validation_digest)
            ),
        },
        Err(error) => GateDecision::Refuse {
            reason: format!(
                "consultation gate 无法安全求值 PlanSignedOff（irRevision={} validationDigest={}…）：{error:#}",
                validation.ir_revision,
                digest_prefix(&validation.validation_digest)
            ),
        },
    }
}

/// Read the current round and its ledger, refusing corrupt or closed state.
///
/// A missing current round admits standalone consultation.  A valid schema-3
/// round admits both before and after plan sign-off because membership is an
/// action-local channel choice rather than task authorization.
#[cfg(feature = "selfhost")]
pub fn consultation_admitted(root: &Path) -> Result<GateDecision> {
    let current = root.join("coordination/runtime/CURRENT-ROUND");
    let source = match fs::read_to_string(&current) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GateDecision::Admit {
                basis: "round=null（CURRENT-ROUND 不存在）".to_string(),
            });
        }
        Err(error) => {
            return Ok(GateDecision::Refuse {
                reason: format!(
                    "consultation gate 无法读取 CURRENT-ROUND {}：{error}",
                    current.display()
                ),
            });
        }
    };
    let round = source.trim();
    if !safe_round(round) {
        return Ok(GateDecision::Refuse {
            reason: format!("consultation gate CURRENT-ROUND 非法：{round:?}"),
        });
    }

    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = match orch_core::read_ledger(&ledger_path) {
        Ok(read) => read,
        Err(error) => {
            return Ok(GateDecision::Refuse {
                reason: format!(
                    "consultation gate 无法读取账本 {}：{error}",
                    ledger_path.display()
                ),
            });
        }
    };
    if let Some((line, error)) = read.bad_lines.first() {
        return Ok(GateDecision::Refuse {
            reason: format!(
                "consultation gate 账本坏行：{} line {}：{}",
                ledger_path.display(),
                line,
                error
            ),
        });
    }
    if orch_core::fold(&read.events).round_closed {
        return Ok(GateDecision::Refuse {
            reason: format!("round={round} 已 RoundClosed；consult writer 不得追加新产物"),
        });
    }
    let contract_schema = if read.events.iter().any(|event| event.kind == "RoundOpened") {
        crate::round::contract_schema_from_events(&read.events, round)
    } else {
        Ok(None)
    };
    match contract_schema {
        Ok(Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)) => {
            let has_validation = read
                .events
                .iter()
                .any(|event| crate::plan::decode_runtime_task_validated(event, round).is_ok());
            let validation = if has_validation {
                match crate::plan::require_validated_round_ir(root, round, &read.events) {
                    Ok(validation) => Some(validation),
                    Err(error) => {
                        return Ok(GateDecision::Refuse {
                            reason: format!(
                                "schema 3 consultation 拒绝损坏的 validated IR：{error:#}"
                            ),
                        })
                    }
                }
            } else {
                None
            };
            let latest_signed = if let Some(validation) = validation.as_ref() {
                match crate::plan::matching_user_plan_signoff(
                    &read.events,
                    round,
                    validation.persisted_revision,
                    &validation.persisted_digest,
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        return Ok(GateDecision::Refuse {
                            reason: format!(
                                "schema 3 consultation 无法验证 latest sign-off：{error:#}"
                            ),
                        })
                    }
                }
            } else {
                false
            };
            if latest_signed {
                if let Err(error) = crate::plan::require_active_round_ir(root, round, &read.events)
                {
                    return Ok(GateDecision::Refuse {
                        reason: format!("schema 3 consultation 拒绝损坏的 signed IR：{error:#}"),
                    });
                }
            }
            return Ok(GateDecision::Admit {
                basis: format!(
                    "round={round} schema=3；consult 编组不属于 task authorization，PlanSignedOff 前后均可调用"
                ),
            });
        }
        Ok(None) => {}
        Ok(Some(version)) => {
            return Ok(GateDecision::Refuse {
                reason: format!("consultation gate 未建模 contract schema {version}"),
            });
        }
        Err(error) => {
            return Ok(GateDecision::Refuse {
                reason: format!("consultation gate RoundOpened 非 canonical：{error:#}"),
            });
        }
    }
    Ok(consultation_gate(&read.events, round))
}

/// Validate, archive and concurrently invoke explicit harness members.
///
/// Protocol events are never written, and a failed member remains one indexed
/// result instead of erasing valid siblings.
pub fn run_consultation(root: &Path, args: &ConsultArgs) -> Result<ConsultOutcome> {
    run_channel_consultation_v3(root, args)
}

fn channel_tuple_json(tuple: &crate::channel::InvocationTuple) -> serde_json::Value {
    serde_json::json!({
        "provider": tuple.provider,
        "model": tuple.model,
        "effort": tuple.effort,
        "mode": tuple.mode,
    })
}

fn channel_consult_facts(rendered: &crate::channel::RenderedInvocationV1) -> serde_json::Value {
    let prepared = rendered.prepared();
    let contract = prepared.driver_contract();
    serde_json::json!({
        "stage": "rendered",
        "alias": prepared.alias(),
        "driver": prepared.driver().as_str(),
        "action": prepared.requested().action.as_str(),
        "configDigest": prepared.config_digest(),
        "requestDigest": prepared.request_digest(),
        "attachmentManifestSha256": prepared.attachment_manifest_digest(),
        "commandDigest": rendered.command_digest(),
        "executable": prepared.executable().display().to_string(),
        "executableIdentityDigest": prepared.executable_identity().sha256(),
        "cwd": rendered.cwd().display().to_string(),
        "fixedHead": prepared.target_head(),
        "requestedTuple": channel_tuple_json(&prepared.requested().tuple),
        "effectiveTuple": channel_tuple_json(prepared.effective()),
        "limits": prepared.limits(),
        "deadlineSecs": rendered.context().deadline_secs,
        "observedTuple": serde_json::Value::Null,
        "observationSource": prepared.observation_source(),
        "receipt": {
            "source": contract.receipt.as_str(),
            "status": "pending",
        },
        "terminal": {
            "source": contract.terminal.as_str(),
            "status": "pending",
        },
    })
}

fn channel_consult_not_started_facts(
    alias: &str,
    snapshot: &crate::harness_config::HarnessConfigSnapshot,
    stage: &str,
    fixed_head: &str,
    cwd: &Path,
    attachment_manifest_digest: &str,
) -> serde_json::Value {
    serde_json::json!({
        "stage": stage,
        "alias": alias,
        "fixedHead": fixed_head,
        "cwd": cwd.display().to_string(),
        "requestDigest": serde_json::Value::Null,
        "configDigest": snapshot.sha256(),
        "configSource": snapshot.source_path().display().to_string(),
        "attachmentManifestSha256": attachment_manifest_digest,
        "receipt": {"status": "not-observed"},
        "terminal": {"status": "not-started"},
    })
}

fn archive_channel_consult_member(
    skeleton: &ConsultationSkeleton,
    outcome: &MemberOutcome,
) -> Result<()> {
    let answer_path = skeleton
        .fusion_dir
        .join(format!("{}-{}.md", outcome.index, outcome.member));
    let answer_bytes = outcome.answer.as_deref().unwrap_or_default().as_bytes();
    write_new_regular(&answer_path, answer_bytes).with_context(|| {
        format!(
            "归档 schema 3 consult raw member answer 失败: {}",
            answer_path.display()
        )
    })?;
    let reread = fs::read(&answer_path)?;
    if reread != answer_bytes {
        bail!("consult raw member answer 写后字节漂移");
    }
    let artifact_sha256 = sha256(&reread);
    let artifact_bytes = u64::try_from(reread.len()).context("consult artifact 长度溢出")?;
    let facts = outcome.channel_facts.as_ref();
    let invocation = facts
        .and_then(|facts| {
            Some((
                facts.get("alias")?.as_str()?,
                facts.get("fixedHead")?.as_str()?,
                facts.get("cwd")?.as_str()?,
                facts.get("requestDigest")?.as_str()?,
                facts.get("configDigest")?.as_str()?,
                facts.get("attachmentManifestSha256")?.as_str()?,
            ))
        })
        .map(|(alias, head, cwd, request, config, attachments)| {
            ConsultMemberManifestV3::new(
                alias,
                head,
                cwd,
                request,
                config,
                attachments,
                &artifact_sha256,
                artifact_bytes,
            )
        })
        .transpose()?;
    let manifest = serde_json::json!({
        "schemaVersion": 3,
        "member": outcome.member,
        "index": outcome.index,
        "status": member_status_name(outcome.status),
        "exitCode": outcome.exit_code,
        "answerExtraction": outcome.answer_extraction,
        "failureClass": outcome.failure_class.map(failure_class_name),
        "reason": outcome.reason,
        "durationSecs": outcome.duration_secs,
        "observedModel": outcome.observed_model,
        "artifact": {
            "path": answer_path.file_name().and_then(|name| name.to_str()),
            "sha256": artifact_sha256,
            "bytes": artifact_bytes,
        },
        "invocation": invocation,
        "channelFacts": outcome.channel_facts,
    });
    let manifest_path = skeleton.fusion_dir.join(format!(
        "{}-{}.manifest.json",
        outcome.index, outcome.member
    ));
    write_new_regular(
        &manifest_path,
        &serde_json::to_vec_pretty(&manifest).context("序列化 consult member manifest 失败")?,
    )
    .with_context(|| {
        format!(
            "归档 schema 3 consult member manifest 失败: {}",
            manifest_path.display()
        )
    })
}

/// Atomically publish complete immutable artifact bytes without replacing an existing path.
pub(crate) fn write_new_regular(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("consult artifact 缺 parent")?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        bail!("consult artifact parent 必须是 real directory");
    }
    // Publish complete bytes without replacing an existing immutable artifact.
    // The manifest is installed only after its answer is durable and single-link.
    let temporary = parent.join(format!(".consult-artifact-{}", ulid::Ulid::new()));
    let mut owned_temporary = false;
    let install = (|| -> Result<()> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
        owned_temporary = true;
        file.write_all(bytes)?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            bail!("consult artifact 必须是 regular single-link file");
        }
        drop(file);
        fs::hard_link(&temporary, path).context("publish complete consult artifact without overwrite")?;
        Ok(())
    })();
    let cleanup = if owned_temporary { fs::remove_file(&temporary) } else { Ok(()) };
    install?;
    cleanup?;
    let after = fs::symlink_metadata(path)?;
    if after.file_type().is_symlink() || !after.is_file() || after.nlink() != 1 {
        bail!("consult artifact 写后不再是 regular single-link file");
    }
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Default)]
struct ChannelConsultTerminalEvidenceV1 {
    turn_ended: bool,
    final_text: Option<String>,
    final_text_sha256: Option<String>,
    usage: Option<serde_json::Value>,
    exact_reason: Option<String>,
}

fn channel_consult_terminal_evidence_v1(
    driver: crate::harness::HarnessId,
    _action_id: &str,
    stdout: &str,
) -> Result<ChannelConsultTerminalEvidenceV1> {
    let mut evidence = ChannelConsultTerminalEvidenceV1::default();
    let mut last_session_id: Option<String> = None;
    let mut last_answer: Option<(Option<String>, String)> = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event_type = value.get("type").and_then(serde_json::Value::as_str);
        if let Some(session) = value
            .get("sessionID")
            .or_else(|| value.get("sessionId"))
            .or_else(|| value.get("thread_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|session| !session.trim().is_empty())
        {
            last_session_id = Some(session.to_string());
        }
        let answer_candidate = match driver {
            crate::harness::HarnessId::OpenCode if event_type == Some("text") => value
                .pointer("/part/text")
                .and_then(serde_json::Value::as_str),
            crate::harness::HarnessId::Codex
                if event_type == Some("item.completed")
                    && value
                        .pointer("/item/type")
                        .and_then(serde_json::Value::as_str)
                        == Some("agent_message") =>
            {
                value
                    .pointer("/item/text")
                    .and_then(serde_json::Value::as_str)
            }
            _ => None,
        };
        if let Some(text) = answer_candidate.filter(|text| !text.trim().is_empty()) {
            last_answer = Some((last_session_id.clone(), text.to_string()));
        }
        let terminal = match driver {
            crate::harness::HarnessId::OpenCode => {
                event_type == Some("step_finish")
                    && value
                        .pointer("/part/type")
                        .and_then(serde_json::Value::as_str)
                        == Some("step-finish")
                    && value
                        .pointer("/part/reason")
                        .and_then(serde_json::Value::as_str)
                        == Some("stop")
            }
            crate::harness::HarnessId::Codex => event_type == Some("turn.completed"),
            crate::harness::HarnessId::Claude => {
                event_type == Some("result")
                    && value.get("subtype").and_then(serde_json::Value::as_str) == Some("success")
                    && value.get("is_error").and_then(serde_json::Value::as_bool) == Some(false)
            }
            crate::harness::HarnessId::Cursor | crate::harness::HarnessId::CodeBuddy => {
                event_type == Some("result")
            }
            crate::harness::HarnessId::Mimo => event_type == Some("step_finish"),
            crate::harness::HarnessId::Pi => event_type == Some("pi.terminal"),
            crate::harness::HarnessId::ZCode => event_type == Some("zcode.terminal"),
            crate::harness::HarnessId::Dsh => event_type == Some("dsh.terminal"),
            crate::harness::HarnessId::Agy
            | crate::harness::HarnessId::Dclaw
            | crate::harness::HarnessId::SmartClaw => false,
        };
        if !terminal {
            continue;
        }
        if evidence.turn_ended {
            bail!("schema 3 consult driver emitted duplicate terminal frames");
        }
        let terminal_session = value
            .get("sessionId")
            .or_else(|| value.get("sessionID"))
            .or_else(|| value.get("thread_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|session| !session.trim().is_empty())
            .map(str::to_string)
            .or_else(|| last_session_id.clone());
        evidence.turn_ended = true;
        evidence.final_text_sha256 = value
            .get("finalTextSha256")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        evidence.final_text = match driver {
            crate::harness::HarnessId::OpenCode | crate::harness::HarnessId::Codex => {
                match last_answer.as_ref() {
                    Some((answer_session, text)) if answer_session == &terminal_session => {
                        Some(text.clone())
                    }
                    Some(_) => bail!("schema 3 consult answer session drifted"),
                    None => None,
                }
            }
            crate::harness::HarnessId::Claude
            | crate::harness::HarnessId::Cursor
            | crate::harness::HarnessId::CodeBuddy => value
                .get("result")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
            crate::harness::HarnessId::Pi
            | crate::harness::HarnessId::ZCode
            | crate::harness::HarnessId::Dsh => {
                let text = value.get("finalText").and_then(serde_json::Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .context("managed consult terminal lacks a complete finalText")?;
                if evidence.final_text_sha256.is_none() {
                    bail!("managed consult terminal lacks finalTextSha256");
                }
                Some(text.to_string())
            }
            crate::harness::HarnessId::Mimo => value
                .pointer("/part/text")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
            _ => None,
        };
        evidence.usage = value
            .get("usage")
            .filter(|usage| usage.is_object())
            .cloned();
        evidence.exact_reason = value
            .get("exactReason")
            .and_then(serde_json::Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .map(str::to_string)
            .or_else(|| Some(format!("{} terminal frame", driver.as_str())));
    }
    Ok(evidence)
}

#[derive(Debug)]
struct ReadyConsultMemberV3 {
    index: usize,
    alias: String,
    action_id: String,
    driver: crate::harness::HarnessId,
    effective_model: Option<String>,
    contract: crate::harness::DriverContract,
    rendered: Option<crate::channel::RenderedInvocationV1>,
    outcome: MemberOutcome,
}

fn consult_output_is_tool_only(stdout: &str) -> bool {
    let mut saw_tool = false;
    let mut saw_final = false;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = value.get("type").and_then(serde_json::Value::as_str);
        saw_tool |= matches!(
            kind,
            Some(
                "tool_use"
                    | "tool_result"
                    | "tool/call"
                    | "tool/result"
                    | "tool_execution_start"
                    | "tool_execution_end"
            )
        ) || (kind == Some("item.completed")
            && value
                .pointer("/item/type")
                .and_then(serde_json::Value::as_str)
                == Some("command_execution"));
        saw_final |= match kind {
            Some("text") => value
                .pointer("/part/text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
            Some("item.completed") => {
                value
                    .pointer("/item/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("agent_message")
                    && value
                        .pointer("/item/text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| !text.trim().is_empty())
            }
            Some("result") => value
                .get("result")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
            Some("step_finish") => value
                .pointer("/part/text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty()),
            _ => false,
        };
    }
    saw_tool && !saw_final
}

fn finish_channel_consult_member_v3(
    log_dir: &Path,
    ready: ReadyConsultMemberV3,
    duration_secs: u64,
    execution: std::thread::Result<Result<crate::channel::ChannelExecution>>,
) -> Result<MemberOutcome> {
    let ReadyConsultMemberV3 {
        index,
        alias,
        action_id,
        driver,
        effective_model,
        contract,
        rendered: _,
        mut outcome,
    } = ready;
    outcome.duration_secs = duration_secs;
    let output = match execution {
        Err(_) => {
            outcome.failure_class = Some(FailureClass::Protocol);
            outcome.reason = Some("channel runner panicked".to_string());
            if let Some(facts) = outcome.channel_facts.as_mut() {
                facts["stage"] = serde_json::json!("runner-panicked");
                facts["terminal"]["status"] = serde_json::json!("protocol-error");
            }
            return Ok(outcome);
        }
        Ok(Err(error)) => {
            outcome.failure_class = Some(FailureClass::Unknown);
            outcome.reason = Some(format!("{error:#}"));
            if let Some(facts) = outcome.channel_facts.as_mut() {
                facts["stage"] = serde_json::json!("preflight-or-execution-rejected");
                facts["receipt"]["status"] = serde_json::json!("not-observed");
                facts["terminal"]["status"] = serde_json::json!("execution-error");
            }
            return Ok(outcome);
        }
        Ok(Ok(output)) => output,
    };

    outcome.exit_code = output.exit_code();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout_path = log_dir.join(format!("member-{index}-{alias}.stdout.log"));
    let stderr_path = log_dir.join(format!("member-{index}-{alias}.stderr.log"));
    write_new_regular(&stdout_path, redact::redact_full(&stdout).as_bytes())?;
    write_new_regular(&stderr_path, redact::redact_full(&stderr).as_bytes())?;
    let driver_failure = crate::channel::classify_driver_failure(
        driver, &stdout, &stderr, output.reason, output.exit_code(),
    );
    if let Some(facts) = outcome.channel_facts.as_mut() {
        facts["execution"] = output.facts();
        facts["diagnostic"] = serde_json::json!({
            "failure": driver_failure,
            "redactedStdoutLog": stdout_path,
            "redactedStderrLog": stderr_path,
        });
    }
    if !output.process_group_terminated || output.status.is_none() {
        outcome.failure_class = Some(driver_failure.as_ref().map(|failure| failure.class())
            .unwrap_or(FailureClass::ObservationFailed));
        outcome.reason = Some("invocation termination unproven; HOLD with private raw captures".into());
        if let Some(facts) = outcome.channel_facts.as_mut() {
            facts["stage"] = serde_json::json!("unclosed");
            facts["terminal"] = serde_json::json!({
                "status": "unknown", "managedScopeTerminated": false,
                "turnEnded": false, "gcAuthorized": false,
            });
        }
        return Ok(outcome);
    }

    if driver == crate::harness::HarnessId::SmartClaw && output.native_final.as_ref()
        .is_none_or(|native| native["nativeTerminated"] != true) {
        outcome.failure_class = Some(FailureClass::ObservationFailed);
        outcome.reason = Some("SmartClaw client ended but native termination is unproven; final projection unavailable; HOLD".into());
        if let Some(facts) = outcome.channel_facts.as_mut() {
            facts["stage"] = serde_json::json!("unclosed");
            facts["terminal"] = serde_json::json!({
                "status": "unknown", "managedScopeTerminated": false, "turnEnded": false,
                "gcAuthorized": false, "nativeFinal": output.native_final.as_ref().map(crate::channel::smartclaw::without_text),
            });
        }
        return Ok(outcome);
    }

    let mut answer = extract_member_answer(&stdout);
    let native_answer_authoritative = driver == crate::harness::HarnessId::SmartClaw
        && output.native_final_text().is_some()
        && output.reason == crate::channel::ChannelExitReason::Exited
        && (output.exit_code() == Some(0) || crate::harness::wrapper_exit_is_transport_eof(output.exit_code()));
    let evidence = adapter::extract_execution_evidence(driver.as_str(), &stdout_path);
    let terminal_result = if driver == crate::harness::HarnessId::SmartClaw {
        // The raw payload concatenates process text. It is preserved in raw
        // captures/logs and is never substituted for a missing native final.
        answer = ExtractedAnswer { text: String::new(), extraction: AnswerExtraction::Structured };
        Ok(ChannelConsultTerminalEvidenceV1 {
            turn_ended: true,
            final_text: output.native_final_text().map(str::to_string),
            final_text_sha256: output.native_final.as_ref().and_then(|native|
                native.pointer("/final/sha256").and_then(serde_json::Value::as_str).map(str::to_string)),
            exact_reason: Some(if output.native_final_text().is_some() { "native-final" }
                else { "native-ended-without-valid-final" }.into()),
            usage: None,
        })
    } else {
        channel_consult_terminal_evidence_v1(driver, &action_id, &stdout)
    };
    let terminal_evidence = match terminal_result {
        Ok(evidence) => evidence,
        Err(error) => {
            outcome.answer = (!answer.text.is_empty()).then(|| answer.text.clone());
            outcome.answer_extraction = Some(answer_extraction_label(&answer).to_string());
            outcome.failure_class = Some(FailureClass::Protocol);
            outcome.reason = Some(format!("driver terminal protocol error: {error:#}"));
            if let Some(facts) = outcome.channel_facts.as_mut() {
                facts["stage"] = serde_json::json!("terminal-protocol-error");
                facts["receipt"] = serde_json::json!({
                    "source": contract.receipt.as_str(),
                    "status": if contract.receipt == crate::harness::CapabilitySource::Absent {
                        "unsupported"
                    } else {
                        "unknown"
                    },
                });
                facts["terminal"] = serde_json::json!({
                    "source": contract.terminal.as_str(),
                    "status": "protocol-error",
                });
            }
            return Ok(outcome);
        }
    };
    if let Some(final_text) = terminal_evidence.final_text.as_ref() {
        answer = ExtractedAnswer {
            text: final_text.clone(),
            extraction: AnswerExtraction::Structured,
        };
    }
    outcome.answer = (!answer.text.is_empty()).then(|| answer.text.clone());
    outcome.answer_extraction = Some(answer_extraction_label(&answer).to_string());
    outcome.usage = terminal_evidence
        .usage
        .clone()
        .or_else(|| evidence.usage.clone());
    let observed = evidence.observed_model.clone();
    outcome.observed_model = observed.clone();
    let model_match = match (observed.as_deref(), effective_model.as_deref()) {
        (Some(observed), Some(effective)) if crate::probe::model_matches(observed, effective) => {
            "matched"
        }
        (Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    };
    let model_mismatch = model_match == "mismatch";
    let wrapper_timeout = contract.wrapper.is_some() && crate::harness::WRAPPER_EXIT_CODES.iter()
        .any(|entry| Some(entry.code) == output.exit_code() && entry.state == "timedOut");
    let timed_out = output.reason == crate::channel::ChannelExitReason::HardDeadline || wrapper_timeout;
    let usable_answer =
        answer.extraction != AnswerExtraction::RawTranscript && !answer.text.trim().is_empty();
    let final_text =
        (terminal_evidence.turn_ended && terminal_evidence.final_text.is_none() && usable_answer)
            .then(|| answer.text.clone())
            .or_else(|| terminal_evidence.final_text.clone());
    let final_text_sha256 = terminal_evidence.final_text_sha256.clone();
    let final_text = final_text_sha256.is_none().then_some(final_text).flatten();
    let mut terminal =
        match crate::harness::classify_terminal_observation_with_wrapper_codes(crate::harness::TerminalObservation {
            capability: contract.terminal,
            // This observation concerns the authoritative native job. The
            // socket client's original exit is retained in execution facts.
            exit_code: if native_answer_authoritative { None } else { output.exit_code() },
            exact_reason: terminal_evidence
                .exact_reason
                .unwrap_or_else(|| "no trusted driver terminal frame observed".to_string()),
            turn_ended: terminal_evidence.turn_ended,
            final_text,
            final_text_sha256,
            output_path: None,
            output_sha256: None,
            usage: outcome.usage.clone(),
            usage_absent_reason: None,
            managed_scope_terminated: true,
            activity_seen: !stdout.is_empty(),
            authenticated_cancel: false,
        }, contract.wrapper.is_some()) {
            Ok(terminal) => terminal,
            Err(error) => {
                outcome.failure_class = Some(FailureClass::Protocol);
                outcome.reason = Some(format!("terminal envelope protocol error: {error:#}"));
                if let Some(facts) = outcome.channel_facts.as_mut() {
                    facts["stage"] = serde_json::json!("terminal-protocol-error");
                    facts["terminal"] = serde_json::json!({
                        "source": contract.terminal.as_str(),
                        "status": "protocol-error",
                    });
                }
                return Ok(outcome);
            }
        };
    if output.reason == crate::channel::ChannelExitReason::HardDeadline {
        // The runtime winner, not a synthetic OS exit code, owns this state.
        terminal.state = crate::harness::TerminalState::TimedOut;
        terminal.exact_reason = "runtime hard deadline reached".into();
        terminal.turn_ended = false;
        terminal.final_text_sha256 = None;
    }
    let tool_only = terminal_evidence.final_text.is_none() && consult_output_is_tool_only(&stdout);
    let validity = classify_consult_answer_v3(&ConsultAnswerInputV3::from_terminal(
        terminal.clone(),
        answer.text.clone(),
        tool_only,
        answer.extraction == AnswerExtraction::RawTranscript,
    ));
    let receipt_status = if contract.receipt == crate::harness::CapabilitySource::Absent {
        "unsupported"
    } else {
        "unknown"
    };
    if let Some(facts) = outcome.channel_facts.as_mut() {
        facts["stage"] = serde_json::json!("completed");
        facts["observedTuple"] = serde_json::json!({
            "provider": serde_json::Value::Null,
            "model": observed,
            "effort": serde_json::Value::Null,
            "mode": serde_json::Value::Null,
            "modelMatch": model_match,
        });
        facts["receipt"] = serde_json::json!({
            "source": contract.receipt.as_str(),
            "status": receipt_status,
            "reason": if receipt_status == "unsupported" {
                "driver declares no mechanical receipt"
            } else {
                "synchronous consult produced no trusted acceptance receipt"
            },
        });
        facts["terminal"] = serde_json::json!({
            "source": contract.terminal.as_str(),
            "answerAuthority": if native_answer_authoritative { "bound-native-final" } else { "driver-terminal" },
            "status": if terminal.mechanical_terminal_absent {
                "unsupported"
            } else {
                terminal.state.as_str()
            },
            "exitCode": output.exit_code(),
            "stdoutBytes": output.stdout.len(),
            "stderrBytes": output.stderr.len(),
            "turnEnded": terminal.turn_ended,
            "finalTextSha256": terminal.final_text_sha256,
            "toolOnly": tool_only,
            "mechanicalTerminalAbsent": terminal.mechanical_terminal_absent,
            "managedScopeTerminated": terminal.managed_scope_terminated,
        });
    }
    if timed_out {
        outcome.status = MemberStatus::TimedOut;
        outcome.failure_class = Some(FailureClass::Timeout);
        outcome.reason = Some(if output.reason == crate::channel::ChannelExitReason::HardDeadline {
            "member exceeded configured local wall limit"
        } else { "code-owned wrapper reported timeout; upstream termination is a separate fact" }.into());
    } else if native_answer_authoritative && !model_mismatch && validity == ConsultAnswerValidityV3::Valid {
        outcome.status = MemberStatus::Ok;
    } else if let Some(failure) = driver_failure {
        outcome.failure_class = Some(failure.class());
        outcome.reason = Some(failure.summary().to_string());
    } else if !model_mismatch && validity == ConsultAnswerValidityV3::Valid {
        outcome.status = MemberStatus::Ok;
    } else {
        outcome.failure_class = Some(if model_mismatch || output.success() {
            FailureClass::Protocol
        } else {
            FailureClass::Unknown
        });
        outcome.reason = Some(if model_mismatch {
            "observed model 与 effective model 不一致".into()
        } else if let ConsultAnswerValidityV3::Invalid(reason) = validity {
            reason
        } else {
            format!("provider exited {:?}", output.exit_code())
        });
    }
    Ok(outcome)
}

fn run_channel_consultation_v3(root: &Path, args: &ConsultArgs) -> Result<ConsultOutcome> {
    let decision = consultation_admitted(root)?;
    if let GateDecision::Refuse { reason } = &decision {
        #[cfg(feature = "selfhost")]
        record_refusal(root, &decision)
            .with_context(|| format!("记录 consultation refusal 失败（{reason}）"))?;
        bail!(reason.clone());
    }
    let explicit_members = ExplicitConsultMembers::new(args.harnesses.clone())?;
    let limits = ConsultLimits {
        per_member_timeout_secs: args
            .member_timeout_secs
            .unwrap_or(CHANNEL_V3_MEMBER_TIMEOUT_SECS),
        total_wall_secs: args.total_wall_secs.unwrap_or(CHANNEL_V3_TOTAL_WALL_SECS),
        max_members: CHANNEL_V3_MAX_MEMBERS,
    }
    .validate("schema 3 channel consult limits")?;

    let canonical_root = fs::canonicalize(root)?;
    let fixed_head = crate::gitx::rev_parse(&canonical_root, "HEAD^{commit}")?;
    let mut manifest_paths = Vec::with_capacity(args.attachments.len() + 1);
    manifest_paths.push(if args.question.is_absolute() {
        args.question.clone()
    } else {
        canonical_root.join(&args.question)
    });
    manifest_paths.extend(args.attachments.iter().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            canonical_root.join(path)
        }
    }));
    let manifest_refs = manifest_paths
        .iter()
        .map(PathBuf::as_path)
        .collect::<Vec<_>>();
    let attachment_manifest = crate::channel::capture_attachment_manifest_v1(&manifest_refs)?;
    let forbidden = forbidden_artifact_patterns(root)?;
    let (question, attachments) =
        project_channel_consult_inputs(&canonical_root, &attachment_manifest, &forbidden)?;
    let prompt = build_fusion_prompt(&question.text, &attachments);
    let orch_executable = std::env::current_exe().context("解析当前 orch executable 失败")?;
    let round = current_round_for_log(root).unwrap_or_else(|| "manual".to_string());
    let snapshot = crate::harness_config::load_harness_config_snapshot(root)?;

    let skeleton = create_consultation_skeleton(root)?;
    archive_request(&skeleton, &question, &attachments)?;
    let log_dir = skeleton.dir.join("adapter-logs");
    fs::create_dir_all(&log_dir)?;

    let mut ready = Vec::with_capacity(explicit_members.as_slice().len());
    let mut outcomes = (0..explicit_members.as_slice().len())
        .map(|_| None)
        .collect::<Vec<Option<MemberOutcome>>>();
    for (index, alias) in explicit_members.as_slice().iter().cloned().enumerate() {
        let mut outcome = MemberOutcome::new(index, alias.clone(), MemberStatus::Failed);
        outcome.worktree = Some(canonical_root.clone());
        let action_id = format!("{}-{index}-{alias}", skeleton.id);
        let rendered = crate::channel::prepare_invocation(
            &snapshot,
            crate::channel::InvocationRequest {
                alias: alias.clone(),
                action: crate::channel::InvocationAction::Consult,
                prompt: prompt.clone(),
                project_root: canonical_root.clone(),
                target_worktree: canonical_root.clone(),
                target_head: fixed_head.clone(),
                attachments: attachment_manifest.clone(),
            },
        )
        .and_then(|prepared| {
            let deadline_secs = prepared.limits().deadline_seconds(
                args.member_timeout_secs,
                CHANNEL_V3_MEMBER_TIMEOUT_SECS,
                limits.total_wall_secs,
            )?;
            crate::channel::render_invocation_v1(
                prepared,
                crate::channel::InvocationContextV1 {
                    action_id: action_id.clone(),
                    wake_id: action_id.clone(),
                    round: round.clone(),
                    task_id: "CONSULT".into(),
                    attempt_id: "CONSULT-A0000".into(),
                    review_output: None,
                    orch_executable: orch_executable.clone(),
                    deadline_secs,
                },
            )
        });
        match rendered {
            Ok(rendered) => {
                let driver = rendered.prepared().driver();
                let effective_model = rendered.prepared().effective().model.clone();
                let contract = rendered.prepared().driver_contract();
                outcome.channel_facts = Some(channel_consult_facts(&rendered));
                ready.push(ReadyConsultMemberV3 {
                    index,
                    alias,
                    action_id,
                    driver,
                    effective_model,
                    contract,
                    rendered: Some(rendered),
                    outcome,
                });
            }
            Err(error) => {
                outcome.failure_class = Some(FailureClass::Unknown);
                outcome.reason = Some(format!("{error:#}"));
                outcome.channel_facts = Some(channel_consult_not_started_facts(
                    &alias,
                    &snapshot,
                    "prepare-or-render-rejected",
                    &fixed_head,
                    &canonical_root,
                    attachment_manifest.sha256(),
                ));
                archive_channel_consult_member(&skeleton, &outcome)?;
                outcomes[index] = Some(outcome);
            }
        }
    }

    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = std::sync::mpsc::channel();
        for mut member in ready {
            let sender = sender.clone();
            scope.spawn(move || {
                    let started = Instant::now();
                    let execution = std::panic::catch_unwind(AssertUnwindSafe(|| {
                        let rendered = member
                            .rendered
                            .take()
                            .context("prepared consult member lost its render")?;
                        crate::channel::preflight_invocation_v1(rendered)
                            .and_then(crate::channel::run_preflighted_invocation_v1)
                    }));
                    let _ = sender.send((member, started.elapsed().as_secs(), execution));
            });
        }
        drop(sender);
        let mut first_error = None;
        for (member, duration_secs, execution) in receiver {
            let index = member.index;
            let finished = finish_channel_consult_member_v3(&log_dir, member, duration_secs, execution)
                .and_then(|outcome| {
                    archive_channel_consult_member(&skeleton, &outcome)?;
                    Ok(outcome)
                });
            match finished {
                Ok(outcome) => outcomes[index] = Some(outcome),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })?;
    let members = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            outcome.with_context(|| format!("consult member {index} 缺稳定结果槽"))
        })
        .collect::<Result<Vec<_>>>()?;
    if members.iter().any(|member| member.channel_facts.as_ref()
        .is_some_and(|facts| facts["stage"] == "unclosed")) {
        bail!("consultation {} remains unclosed (HOLD); stable member artifacts are in {}; no summary/completion or GC authority",
            skeleton.id, skeleton.fusion_dir.display());
    }
    let created_at = now_rfc3339();
    let summary_path = write_root_summary(&skeleton, &members)?;
    let meta = consultation_meta_v3(
        &skeleton,
        explicit_members.as_slice(),
        limits,
        &question,
        &attachments,
        &members,
        current_round_for_log(root),
        &created_at,
        attachment_manifest.sha256(),
        &summary_path,
    );
    write_new_regular(
        &skeleton.dir.join("meta.json"),
        &serde_json::to_vec_pretty(&meta).context("序列化 consultation meta 失败")?,
    )?;
    let members_ok = members
        .iter()
        .filter(|member| member.status == MemberStatus::Ok)
        .count();
    append_consultation_log(
        root,
        &serde_json::json!({
            "ts": created_at,
            "kind": "ConsultationCompleted",
            "id": skeleton.id,
            "round": current_round_for_log(root),
            "questionPath": question.display_path,
            "questionSha256": question.sha256,
            "membersOk": members_ok,
            "membersFailed": members.len() - members_ok,
            "harnesses": explicit_members.as_slice(),
            "summaryPath": root_relative(root, &summary_path),
            "dir": root_relative(root, &skeleton.dir),
            "invocation": "unified-channel-v1",
            "attachmentManifestSha256": attachment_manifest.sha256(),
        }),
    )?;
    Ok(ConsultOutcome {
        id: skeleton.id,
        dir: skeleton.dir,
        members,
        summary_path,
    })
}

#[derive(Debug)]
struct LoadedQuestion {
    display_path: String,
    bytes: Vec<u8>,
    text: String,
    sha256: String,
}

#[derive(Debug)]
struct LoadedAttachment {
    relative_path: String,
    bytes: Vec<u8>,
    text: String,
    sha256: String,
}

fn project_channel_consult_inputs(
    canonical_root: &Path,
    manifest: &crate::channel::AttachmentManifestV1,
    forbidden: &[String],
) -> Result<(LoadedQuestion, Vec<LoadedAttachment>)> {
    let (question_entry, attachment_entries) = manifest
        .entries()
        .split_first()
        .context("schema 3 consult manifest 缺 question snapshot")?;
    let question_text = String::from_utf8(question_entry.bytes().to_vec()).with_context(|| {
        format!(
            "consult question 不是 UTF-8：{}",
            question_entry.path().display()
        )
    })?;
    reject_secret_lines("question", question_entry.path(), &question_text)?;
    let question = LoadedQuestion {
        display_path: question_entry
            .path()
            .strip_prefix(canonical_root)
            .map(normalized_path)
            .unwrap_or_else(|_| question_entry.path().display().to_string()),
        bytes: question_entry.bytes().to_vec(),
        text: question_text,
        sha256: question_entry.sha256().to_string(),
    };

    let mut seen = HashSet::new();
    let mut attachments = Vec::with_capacity(attachment_entries.len());
    for entry in attachment_entries {
        let relative = entry.path().strip_prefix(canonical_root).with_context(|| {
            format!(
                "consult attachment 必须是仓内路径，拒绝仓外文件：{}",
                entry.path().display()
            )
        })?;
        let relative_path = normalized_path(relative);
        let basename = entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if let Some(pattern) = forbidden.iter().find(|pattern| {
            wildcard_matches(pattern, basename) || wildcard_matches(pattern, &relative_path)
        }) {
            bail!(
                "consult attachment 命中 PROJECT-BINDING forbiddenArtifactPatterns {:?}：{}",
                pattern,
                relative_path
            );
        }
        if !seen.insert(relative_path.clone()) {
            bail!("consult attachment 重复：{relative_path}");
        }
        let text = String::from_utf8(entry.bytes().to_vec()).with_context(|| {
            format!("consult attachment 不是 UTF-8：{}", entry.path().display())
        })?;
        reject_secret_lines("attachment", entry.path(), &text)?;
        attachments.push(LoadedAttachment {
            relative_path,
            bytes: entry.bytes().to_vec(),
            text,
            sha256: entry.sha256().to_string(),
        });
    }
    Ok((question, attachments))
}

fn reject_secret_lines(kind: &str, path: &Path, text: &str) -> Result<()> {
    for (offset, line) in text.lines().enumerate() {
        if redact::has_secret(line) {
            bail!(
                "consult {kind} 含敏感信息，拒发：{} line {}",
                path.display(),
                offset + 1
            );
        }
    }
    Ok(())
}

fn forbidden_artifact_patterns(root: &Path) -> Result<Vec<String>> {
    let path = root.join("coordination/PROJECT-BINDING.yaml");
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !crate::has_selfhost_state(root)? => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("读取 PROJECT-BINDING 失败：{}", path.display())),
    };
    let value: serde_yaml::Value = serde_yaml::from_str(&source)
        .with_context(|| format!("解析 PROJECT-BINDING 失败：{}", path.display()))?;
    let patterns = value
        .get("data")
        .and_then(|data| data.get("forbiddenArtifactPatterns"))
        .and_then(serde_yaml::Value::as_sequence)
        .context("PROJECT-BINDING 缺 data.forbiddenArtifactPatterns 数组")?;
    patterns
        .iter()
        .map(|pattern| {
            pattern
                .as_str()
                .map(str::to_string)
                .context("PROJECT-BINDING forbiddenArtifactPatterns 必须全为字符串")
        })
        .collect()
}

fn build_fusion_prompt(question: &str, attachments: &[LoadedAttachment]) -> String {
    let mut prompt = String::from(
        "You are an independent, read-only planning consultant. Inspect the repository when useful, but do not edit files, commit, push, or perform network actions. Answer the planning question with concrete reasoning and explicitly call out uncertainty.\n\n# Question\n\n",
    );
    prompt.push_str(question);
    if !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    if !attachments.is_empty() {
        prompt.push_str("\n# Archived attachments\n");
    }
    for attachment in attachments {
        prompt.push_str(&format!("\n## {}\n", attachment.relative_path));
        let fence = markdown_fence(&attachment.text);
        prompt.push_str(&fence);
        prompt.push_str("text\n");
        prompt.push_str(&attachment.text);
        if !attachment.text.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str(&fence);
        prompt.push('\n');
    }
    prompt
}

fn archive_request(
    skeleton: &ConsultationSkeleton,
    question: &LoadedQuestion,
    attachments: &[LoadedAttachment],
) -> Result<()> {
    write_new_regular(&skeleton.request_dir.join("question.md"), &question.bytes)
        .context("归档 consultation question 失败")?;
    for attachment in attachments {
        let target = skeleton
            .request_dir
            .join("attachments")
            .join(&attachment.relative_path);
        let parent = target.parent().context("attachment 归档路径无父目录")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 attachment 归档目录失败：{}", parent.display()))?;
        write_new_regular(&target, &attachment.bytes)
            .with_context(|| format!("归档 attachment 失败：{}", target.display()))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn consultation_meta_v3(
    skeleton: &ConsultationSkeleton,
    harnesses: &[String],
    limits: ConsultLimits,
    question: &LoadedQuestion,
    attachments: &[LoadedAttachment],
    members: &[MemberOutcome],
    round: Option<String>,
    created_at: &str,
    attachment_manifest_digest: &str,
    summary_path: &Path,
) -> serde_json::Value {
    let attachments = attachments
        .iter()
        .map(|attachment| {
            serde_json::json!({
                "path": attachment.relative_path,
                "sha256": attachment.sha256,
                "bytes": attachment.bytes.len(),
            })
        })
        .collect::<Vec<_>>();
    let members = members
        .iter()
        .map(|member| {
            serde_json::json!({
                "index": member.index,
                "harness": member.member,
                "status": member_status_name(member.status),
                "exitCode": member.exit_code,
                "answerExtraction": member.answer_extraction,
                "failureClass": member.failure_class.map(failure_class_name),
                "reason": member.reason,
                "durationSecs": member.duration_secs,
                "usage": member.usage,
                "observedModel": member.observed_model,
                "artifactPath": format!("fusion/{}-{}.md", member.index, member.member),
                "manifestPath": format!(
                    "fusion/{}-{}.manifest.json",
                    member.index, member.member
                ),
                "channelFacts": member.channel_facts,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "schemaVersion": 3,
        "id": skeleton.id,
        "createdAt": created_at,
        "round": round,
        "membership": {
            "source": "explicit",
            "harnesses": harnesses,
        },
        "question": {
            "path": question.display_path,
            "sha256": question.sha256,
            "bytes": question.bytes.len(),
        },
        "attachments": attachments,
        "attachmentManifestSha256": attachment_manifest_digest,
        "limits": {
            "perMemberTimeoutSecs": limits.per_member_timeout_secs,
            "totalWallSecs": limits.total_wall_secs,
            "maxMembers": limits.max_members,
        },
        "members": members,
        "summaryPath": summary_path
            .strip_prefix(&skeleton.dir)
            .map(normalized_path)
            .unwrap_or_else(|_| summary_path.display().to_string()),
    })
}

fn write_root_summary(
    skeleton: &ConsultationSkeleton,
    members: &[MemberOutcome],
) -> Result<PathBuf> {
    let mut summary = String::from(
        "# Consultation member index\n\nThis file is an index, not a judge or verdict.\n",
    );
    for member in members {
        summary.push_str(&format!(
            "\n- [{}] {}: {} · raw=fusion/{}-{}.md · manifest=fusion/{}-{}.manifest.json",
            member.index,
            member.member,
            member_status_name(member.status),
            member.index,
            member.member,
            member.index,
            member.member,
        ));
        if let Some(reason) = member.reason.as_deref() {
            summary.push_str(&format!(" · reason={}", redact::redact_full(reason)));
        }
        summary.push('\n');
    }
    let path = skeleton.dir.join("summary.md");
    write_new_regular(&path, summary.as_bytes())?;
    Ok(path)
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn markdown_fence(text: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn normalized_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn root_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(normalized_path)
        .unwrap_or_else(|_| path.display().to_string())
}

fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut p, mut t, mut star, mut matched) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            matched = t;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            matched += 1;
            t = matched;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn member_status_name(status: MemberStatus) -> &'static str {
    match status {
        MemberStatus::Ok => "ok",
        MemberStatus::Failed => "failed",
        MemberStatus::TimedOut => "timedOut",
    }
}

fn failure_class_name(class: FailureClass) -> &'static str {
    class.as_str()
}

fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// Create only the successful-consultation artifact skeleton. Refusal paths
/// must never call this function.
pub fn create_consultation_skeleton(root: &Path) -> Result<ConsultationSkeleton> {
    let id = ulid::Ulid::new().to_string();
    let dir = root.join(CONSULTATIONS_PATH).join(&id);
    let request_dir = dir.join("request");
    let fusion_dir = dir.join("fusion");
    fs::create_dir_all(&request_dir).with_context(|| {
        format!(
            "创建 consultation request 目录失败: {}",
            request_dir.display()
        )
    })?;
    fs::create_dir_all(&fusion_dir).with_context(|| {
        format!(
            "创建 consultation fusion 目录失败: {}",
            fusion_dir.display()
        )
    })?;
    Ok(ConsultationSkeleton {
        id,
        dir,
        request_dir,
        fusion_dir,
    })
}

/// Append one JSON value as exactly one redacted JSONL record. The log file
/// itself is used as the cross-process advisory lock, so no lock artifact is
/// left beside a refusal.
pub fn append_consultation_log(root: &Path, entry: &serde_json::Value) -> Result<()> {
    let dir = root.join(CONSULTATIONS_PATH);
    fs::create_dir_all(&dir)
        .with_context(|| format!("创建 consultations 目录失败: {}", dir.display()))?;
    let path = dir.join("log.jsonl");
    let serialized = serde_json::to_string(entry).context("序列化 consultation log 失败")?;
    let mut bytes = redact::redact_full(&serialized).into_bytes();
    bytes.push(b'\n');

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开 consultation log 失败: {}", path.display()))?;
    let mut lock = RwLock::new(file);
    let mut guard = lock
        .write()
        .with_context(|| format!("获取 consultation log 写锁失败: {}", path.display()))?;
    guard
        .write_all(&bytes)
        .with_context(|| format!("追加 consultation log 失败: {}", path.display()))?;
    guard
        .sync_data()
        .with_context(|| format!("同步 consultation log 失败: {}", path.display()))?;
    Ok(())
}

/// Record a gate refusal without touching the protocol ledger or creating a
/// per-consultation ULID directory.
#[cfg(feature = "selfhost")]
pub fn record_refusal(root: &Path, decision: &GateDecision) -> Result<()> {
    let GateDecision::Refuse { reason } = decision else {
        bail!("仅 Refuse decision 可写 ConsultationRefused");
    };
    let round = current_round_for_log(root);
    let entry = serde_json::json!({
        "ts": humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        "kind": "ConsultationRefused",
        "round": round,
        "reason": reason,
    });
    append_consultation_log(root, &entry)
}

fn current_round_for_log(root: &Path) -> Option<String> {
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND")).ok()?;
    let round = round.trim();
    safe_round(round).then(|| round.to_string())
}

#[cfg(feature = "selfhost")]
fn digest_prefix(digest: &str) -> &str {
    digest.get(..8).unwrap_or(digest)
}

fn safe_round(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn safe_harness_alias(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn absolute_lexical_path(path: &Path) -> bool {
    let raw = path.as_os_str().as_encoded_bytes();
    path.is_absolute()
        && !(raw.len() > 1 && (raw.ends_with(b"/") || raw.windows(2).any(|pair| pair == b"//")))
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_native_final_survives_transport_eof_but_not_timeout_or_unclosed_native_work() {
        use crate::channel::{ChannelExecution, ChannelExitReason};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let root = crate::util::test_scratch_dir("b327-native-consult-authority");
        let prompt = "bound native request 知🧭";
        let wanted = hex::encode(Sha256::digest(prompt.as_bytes()));
        let final_text = "authoritative complete native answer";
        let create = r#"
import json,sqlite3,sys
db,sid,cwd,prompt,answer,status,has_final=sys.argv[1:]; c=sqlite3.connect(db)
c.executescript('CREATE TABLE cowork_sessions(id TEXT,title TEXT,status TEXT,cwd TEXT); CREATE TABLE cowork_messages(id TEXT,session_id TEXT,type TEXT,content TEXT,metadata TEXT,sequence INTEGER);')
c.execute('INSERT INTO cowork_sessions VALUES(?,?,?,?)',('native-1','multica:'+sid,status,cwd))
c.execute('INSERT INTO cowork_messages VALUES(?,?,?,?,?,?)',('user-1','native-1','user',prompt,'{}',0))
if has_final=='true': c.execute('INSERT INTO cowork_messages VALUES(?,?,?,?,?,?)',('final-1','native-1','assistant',answer,json.dumps({'isFinal':True}),1))
c.commit()
"#;
        for (index,(code,state,has_final,expected)) in [
            (0,"completed",true,MemberStatus::Ok),
            (70,"completed",true,MemberStatus::Ok),
            (71,"completed",true,MemberStatus::Ok),
            (72,"completed",true,MemberStatus::TimedOut),
            (0,"running",true,MemberStatus::Failed),
            (71,"completed",false,MemberStatus::Failed),
        ].into_iter().enumerate() {
            let path = root.join(format!("case-{index}")); fs::create_dir_all(&path).unwrap();
            let action_id = format!("native-{index}"); let session = format!("orch-wake-{action_id}");
            let database = path.join("native.sqlite");
            assert!(Command::new("/usr/bin/python3").args(["-I","-c",create]).arg(&database)
                .arg(&session).arg(&root).arg(prompt).arg(final_text).arg(state)
                .arg(has_final.to_string()).status().unwrap().success());
            let child = Command::new("/bin/sh").args(["-c","printf '%s' '{\"type\":\"truncated'; exit \"$1\"","fixture"])
                .arg(code.to_string()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().unwrap();
            let pid = child.id(); let process = child.wait_with_output().unwrap();
            unsafe extern "C" { fn killpg(group: i32, signal: i32) -> i32; }
            assert_eq!(unsafe { killpg(pid as i32,0) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(3));
            let native = crate::channel::smartclaw::inspect_native_final(&root,&database,&session,&root,
                &wanted,&process.stdout,true).unwrap();
            let stdout_path = path.join("raw.stdout"); let stderr_path = path.join("raw.stderr");
            fs::write(&stdout_path,&process.stdout).unwrap(); fs::write(&stderr_path,&process.stderr).unwrap();
            let execution = ChannelExecution { process_id:pid,status:Some(process.status),stdout:process.stdout,
                stderr:process.stderr,reason:ChannelExitReason::Exited,hard_deadline_secs:30,
                first_frame_after_millis:None,leader_exited_after_millis:Some(1),elapsed_millis:1,
                process_group_terminated:true,observation_errors:vec![],stdout_capture_path:Some(stdout_path),
                stderr_capture_path:Some(stderr_path),native_final:Some(native) };
            let driver = crate::harness::HarnessId::SmartClaw;
            let mut outcome = MemberOutcome::new(index,"native",MemberStatus::Failed);
            outcome.channel_facts=Some(serde_json::json!({}));
            let ready = ReadyConsultMemberV3 {index,alias:"native".into(),action_id,driver,effective_model:None,
                contract:driver.driver_contract(crate::harness::DriverAction::Consult).unwrap(),rendered:None,outcome};
            let log_dir=path.join("logs");fs::create_dir_all(&log_dir).unwrap();
            let result=finish_channel_consult_member_v3(&log_dir,ready,1,Ok(Ok(execution))).unwrap();
            assert_eq!(result.status,expected,"code={code} native={state}/{has_final} result={result:?}");
            assert_eq!(result.exit_code,Some(code));
            if expected==MemberStatus::Ok { assert_eq!(result.answer.as_deref(),Some(final_text)); }
            if state=="running" { assert_eq!(result.channel_facts.unwrap()["stage"],"unclosed"); }
        }
        fs::remove_dir_all(root).unwrap();
    }
    use std::sync::atomic::{AtomicU64, Ordering};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("orch workspace root");
        let seq = ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        orch_root.join("target/test-tmp").join(format!(
            "consult-unit-{name}-{}-{}-{seq}",
            std::process::id(),
            ulid::Ulid::new()
        ))
    }

    fn event(kind: &str, actor: &str, payload: serde_json::Value) -> EventRecord {
        serde_json::from_value(serde_json::json!({
            "eventId": ulid::Ulid::new().to_string(),
            "ts": "2026-07-29T00:00:00Z",
            "actor": actor,
            "type": kind,
            "round": "r90",
            "payload": payload,
        }))
        .unwrap()
    }

    fn validation(revision: u32) -> EventRecord {
        event(
            "TaskValidated",
            "runtime:orch",
            plan::task_validated_payload(revision, DIGEST),
        )
    }

    fn signoff(revision: u32) -> EventRecord {
        event(
            "PlanSignedOff",
            "user",
            serde_json::json!({
                "irRevision": revision,
                "validationDigest": DIGEST,
            }),
        )
    }

    fn write_root_ledger(root: &Path, events: &[EventRecord]) -> Vec<u8> {
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r90")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();
        let mut bytes = Vec::new();
        for event in events {
            serde_json::to_writer(&mut bytes, event).unwrap();
            bytes.push(b'\n');
        }
        fs::write(root.join("coordination/rounds/r90/events.jsonl"), &bytes).unwrap();
        bytes
    }

    #[test]
    fn production_wrapper_covers_delivery_replan_close_and_no_round() {
        let none = temp_root("no-round");
        assert!(matches!(
            consultation_admitted(&none).unwrap(),
            GateDecision::Admit { basis } if basis.contains("round=null")
        ));

        let delivery = temp_root("delivery");
        write_root_ledger(&delivery, &[validation(1), signoff(1)]);
        assert!(matches!(
            consultation_admitted(&delivery).unwrap(),
            GateDecision::Refuse { reason } if reason.contains("PlanSignedOff")
        ));

        let replan = temp_root("replan");
        write_root_ledger(&replan, &[validation(1), signoff(1), validation(2)]);
        let replan_decision = consultation_admitted(&replan).unwrap();
        assert!(
            matches!(replan_decision, GateDecision::Admit { .. }),
            "{replan_decision:?}"
        );

        let closed = temp_root("closed");
        write_root_ledger(
            &closed,
            &[
                validation(1),
                signoff(1),
                event("RoundClosed", "runtime:orch", serde_json::json!({})),
            ],
        );
        assert!(matches!(
            consultation_admitted(&closed).unwrap(),
            GateDecision::Refuse { reason } if reason.contains("RoundClosed")
        ));
    }

    #[test]
    fn bad_ledger_refuses_and_refusal_only_appends_the_consult_log() {
        let bad = temp_root("bad-ledger");
        fs::create_dir_all(bad.join("coordination/runtime")).unwrap();
        fs::create_dir_all(bad.join("coordination/rounds/r90")).unwrap();
        fs::write(bad.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();
        fs::write(
            bad.join("coordination/rounds/r90/events.jsonl"),
            b"{\"eventId\":",
        )
        .unwrap();
        assert!(matches!(
            consultation_admitted(&bad).unwrap(),
            GateDecision::Refuse { reason } if reason.contains("坏行")
        ));

        let root = temp_root("append");
        let ledger = write_root_ledger(&root, &[validation(1), signoff(1)]);
        let decision = consultation_admitted(&root).unwrap();
        record_refusal(&root, &decision).unwrap();
        record_refusal(&root, &decision).unwrap();
        assert_eq!(
            fs::read(root.join("coordination/rounds/r90/events.jsonl")).unwrap(),
            ledger
        );
        let log = fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 2);
        assert!(log
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
        assert!(fs::read_dir(root.join("coordination/consultations"))
            .unwrap()
            .all(|entry| !entry.unwrap().path().is_dir()));
    }

    #[test]
    fn explicit_members_preserve_order_and_reject_rewrites() {
        let members = ExplicitConsultMembers::new(["alpha", "beta"]).unwrap();
        assert_eq!(members.as_slice(), ["alpha", "beta"]);
        assert!(ExplicitConsultMembers::new(["alpha", "alpha"]).is_err());
        assert!(ExplicitConsultMembers::new([" alpha"]).is_err());
        assert!(ExplicitConsultMembers::new(["consult/alpha"]).is_err());
        assert!(ExplicitConsultMembers::new(
            (0..=CHANNEL_V3_MAX_MEMBERS).map(|index| format!("member-{index}"))
        )
        .is_err());
    }

    #[test]
    fn member_manifest_rejects_noncanonical_identity_or_digest() {
        let digest = "a".repeat(64);
        let valid = ConsultMemberManifestV3::new(
            "alpha",
            "0".repeat(40),
            "/repo",
            &digest,
            &digest,
            &digest,
            &digest,
            0,
        )
        .unwrap();
        assert!(valid.matches_artifact(&digest, 0));
        assert!(ConsultMemberManifestV3::new(
            "alpha", "short", "/repo", &digest, &digest, &digest, &digest, 0,
        )
        .is_err());
    }

    #[test]
    fn successful_skeleton_is_ulid_scoped() {
        let root = temp_root("skeleton");
        let first = create_consultation_skeleton(&root).unwrap();
        let second = create_consultation_skeleton(&root).unwrap();
        assert_ne!(first.id, second.id);
        assert!(first.request_dir.is_dir());
        assert!(first.fusion_dir.is_dir());
        assert!(first.dir.starts_with(root.join(CONSULTATIONS_PATH)));
    }

    #[test]
    fn channel_consult_prompt_and_archive_use_the_captured_manifest_bytes() {
        let root = temp_root("channel-snapshot");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let question_path = root.join("question.md");
        let attachment_path = root.join("attachment.md");
        fs::write(&question_path, b"OLD-QUESTION\n").unwrap();
        fs::write(&attachment_path, b"OLD-ATTACHMENT\n").unwrap();

        let manifest = crate::channel::capture_attachment_manifest_v1(&[
            question_path.as_path(),
            attachment_path.as_path(),
        ])
        .unwrap();
        let captured_digest = manifest.sha256().to_string();
        fs::write(&question_path, b"NEW-QUESTION\n").unwrap();
        fs::write(&attachment_path, b"NEW-ATTACHMENT\n").unwrap();

        let (question, attachments) =
            project_channel_consult_inputs(&root, &manifest, &[]).unwrap();
        let prompt = build_fusion_prompt(&question.text, &attachments);
        assert!(prompt.contains("OLD-QUESTION"));
        assert!(prompt.contains("OLD-ATTACHMENT"));
        assert!(!prompt.contains("NEW-QUESTION"));
        assert!(!prompt.contains("NEW-ATTACHMENT"));
        assert_eq!(question.sha256, manifest.entries()[0].sha256());
        assert_eq!(attachments[0].sha256, manifest.entries()[1].sha256());
        assert_eq!(manifest.sha256(), captured_digest);

        let skeleton = create_consultation_skeleton(&root).unwrap();
        archive_request(&skeleton, &question, &attachments).unwrap();
        assert_eq!(
            fs::read(skeleton.request_dir.join("question.md")).unwrap(),
            b"OLD-QUESTION\n"
        );
        assert_eq!(
            fs::read(
                skeleton
                    .request_dir
                    .join("attachments")
                    .join("attachment.md")
            )
            .unwrap(),
            b"OLD-ATTACHMENT\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn channel_consult_projection_rejects_outside_and_forbidden_attachments() {
        let root = temp_root("projection-reject");
        let outside_root = temp_root("projection-outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside_root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let outside_root = fs::canonicalize(outside_root).unwrap();
        let question = root.join("question.md");
        let outside = outside_root.join("outside.md");
        fs::write(&question, b"question\n").unwrap();
        fs::write(&outside, b"outside\n").unwrap();
        let outside_manifest = crate::channel::capture_attachment_manifest_v1(&[
            question.as_path(),
            outside.as_path(),
        ])
        .unwrap();
        let outside_error = project_channel_consult_inputs(&root, &outside_manifest, &[])
            .unwrap_err()
            .to_string();
        assert!(outside_error.contains("仓外文件"), "{outside_error}");

        let forbidden = root.join(".env.local");
        fs::write(&forbidden, b"SAFE_FIXTURE=value\n").unwrap();
        let forbidden_manifest = crate::channel::capture_attachment_manifest_v1(&[
            question.as_path(),
            forbidden.as_path(),
        ])
        .unwrap();
        let forbidden_error =
            project_channel_consult_inputs(&root, &forbidden_manifest, &[".env*".into()])
                .unwrap_err()
                .to_string();
        assert!(
            forbidden_error.contains("forbiddenArtifactPatterns"),
            "{forbidden_error}"
        );
        assert!(!root.join(CONSULTATIONS_PATH).exists());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside_root).unwrap();
    }

    #[test]
    fn consult_artifact_writer_is_no_clobber_and_rejects_a_symlink_parent() {
        let root = temp_root("artifact-no-clobber");
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        let artifact = real.join("answer.md");
        write_new_regular(&artifact, b"first").unwrap();
        assert!(write_new_regular(&artifact, b"second").is_err());
        assert_eq!(fs::read(&artifact).unwrap(), b"first");

        let linked = root.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert!(write_new_regular(&linked.join("other.md"), b"unsafe").is_err());
        assert!(!real.join("other.md").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn secret_scan_fails_closed_and_names_the_source_line() {
        let error = reject_secret_lines(
            "attachment",
            Path::new("notes.md"),
            "ordinary context\nAPI_TOKEN=do-not-send\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("notes.md line 2"), "{error}");
        assert!(
            !error.contains("do-not-send"),
            "secret must not enter errors"
        );
    }

    #[test]
    fn binding_wildcards_cover_env_and_private_key_names() {
        assert!(wildcard_matches(".env*", ".env.production"));
        assert!(wildcard_matches("*.pem", "client.pem"));
        assert!(wildcard_matches("*.key", "nested/signing.key"));
        assert!(!wildcard_matches("*.pem", "safe.txt"));
    }
}


pub const ANSWER_BODY_LIMIT: usize = 64 * 1024;

/// Normalise the measured provider JSON-lines shapes into one answer contract.
///
/// The last known terminal turn wins. If none is recognised, only the first
/// non-empty line can identify an unknown typed stream; later JSON snippets in
/// healthy prose are not evidence that the whole answer is a transcript.
pub fn extract_member_answer(raw: &str) -> ExtractedAnswer {
    let first_non_empty_is_typed_stream = raw
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|value| value.as_object().cloned())
        .and_then(|object| object.get("type").cloned())
        .and_then(|value| value.as_str().map(str::to_owned))
        .is_some_and(|event_type| !event_type.trim().is_empty());

    let mut answer = None;
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event_type = value.get("type").and_then(|value| value.as_str());
        let candidate = match event_type {
            Some("item.completed")
                if value.pointer("/item/type").and_then(|value| value.as_str())
                    == Some("agent_message") =>
            {
                value.pointer("/item/text").and_then(|value| value.as_str())
            }
            Some("result") => value.get("result").and_then(|value| value.as_str()),
            Some("text") => value.pointer("/part/text").and_then(|value| value.as_str()),
            // MiMo's measured terminal event predates consult and remains a
            // supported structured result. OpenCode is intentionally handled
            // by the `type=text` branch above, never by this fallback shape.
            Some("step_finish") => value.pointer("/part/text").and_then(|value| value.as_str()),
            _ => None,
        };
        if let Some(candidate) = candidate.filter(|candidate| !candidate.trim().is_empty()) {
            answer = Some(candidate.to_string());
        }
    }
    let (text, recognised_terminal) = match answer {
        Some(answer) => (answer, true),
        None => (raw.to_string(), false),
    };
    let extraction = if text.len() > ANSWER_BODY_LIMIT {
        AnswerExtraction::RawTranscript
    } else if recognised_terminal {
        AnswerExtraction::Structured
    } else if first_non_empty_is_typed_stream {
        AnswerExtraction::RawTranscript
    } else {
        AnswerExtraction::PlainText
    };
    ExtractedAnswer { text, extraction }
}

/// Standalone admission never treats a malformed selfhost marker as absence.
#[cfg(not(feature = "selfhost"))]
pub fn consultation_admitted(root: &Path) -> Result<GateDecision> {
    if crate::has_selfhost_state(root)? {
        Ok(GateDecision::Refuse { reason: "project contains selfhost state; use an orch build with --features selfhost".to_owned() })
    } else {
        Ok(GateDecision::Admit { basis: "standalone Git project with local harness configuration".to_owned() })
    }
}
