//! plan 期多模型咨询（r56/B165，H30）：阶段门、preset 配置、咨询产物层与驱动。
//! 背景：design/04 的 committee 预设（N 独立分析者 → synthesizer → user）自 r29/B54 起
//! 只有投票纯内核、无扇出接线；TASKBOOK-r46-r48 的 D3 记过「单一验证面有盲区」。
//! 本模块把它具体化到方案期：签核前可咨询，`PlanSignedOff` 落账即机械冻结。
//! （planner 预置占位：lib.rs 声明先行入库，B165 在本文件内实现，勿动 lib.rs——frozenPaths。）

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;
use orch_core::EventRecord;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{adapter, failure::FailureClass, fusion, judge, plan, redact};

const PRESETS_PATH: &str = "coordination/consult/presets.yaml";
const CONSULTATIONS_PATH: &str = "coordination/consultations";

/// Whether the current protocol state permits a plan-phase consultation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum GateDecision {
    Admit { basis: String },
    Refuse { reason: String },
}

/// Limits shared by the preset loader and the fusion runner.
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

/// Durable-data-neutral result shared by the fusion runner and judge/driver.
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
        }
    }
}

/// How synthesis is handled after the fusion has settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeMode {
    Planner,
    Adapter(String),
    None,
}

impl JudgeMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "planner" => Ok(Self::Planner),
            "none" => Ok(Self::None),
            adapter if safe_config_name(adapter) => Ok(Self::Adapter(adapter.to_string())),
            _ => bail!("consult judge 名非法：{value:?}"),
        }
    }
}

impl Default for JudgeMode {
    fn default() -> Self {
        Self::Planner
    }
}

/// Inputs accepted by the complete consultation driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultArgs {
    pub question: PathBuf,
    pub preset: String,
    pub attachments: Vec<PathBuf>,
    pub judge: JudgeMode,
    pub member_timeout_secs: Option<u64>,
    pub total_wall_secs: Option<u64>,
}

impl Default for ConsultArgs {
    fn default() -> Self {
        Self {
            question: PathBuf::new(),
            preset: "default".to_string(),
            attachments: Vec::new(),
            judge: JudgeMode::Planner,
            member_timeout_secs: None,
            total_wall_secs: None,
        }
    }
}

/// Durable location and terminal summary returned to the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsultOutcome {
    pub id: String,
    pub dir: PathBuf,
    pub preset: String,
    pub members: Vec<MemberOutcome>,
    pub judge_status: judge::JudgeStatus,
    pub judge_spawns: usize,
}

/// One resolved preset after defaults and per-preset overrides are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultPreset {
    pub name: String,
    pub fusion: Vec<String>,
    pub limits: ConsultLimits,
    pub allow_duplicates: bool,
}

impl ConsultPreset {
    pub fn members(&self) -> Vec<FusionMember> {
        self.fusion
            .iter()
            .enumerate()
            .map(|(index, member)| FusionMember::new(index, member))
            .collect()
    }
}

/// Validated preset collection. Source order is retained for deterministic UX.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultPresets {
    pub presets: Vec<ConsultPreset>,
}

impl ConsultPresets {
    pub fn get(&self, name: &str) -> Option<&ConsultPreset> {
        self.presets.iter().find(|preset| preset.name == name)
    }
}

/// Paths allocated for a successful consultation before any member is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultationSkeleton {
    pub id: String,
    pub dir: PathBuf,
    pub request_dir: PathBuf,
    pub fusion_dir: PathBuf,
}

/// Pure gate fold. Only canonical production `TaskValidated` events participate
/// in the freeze decision; sign-off matching remains delegated to plan.rs.
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
            basis: format!("round={round} 已 RoundClosed，无在飞落地期"),
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

/// Read the current round and its ledger, refusing rather than propagating an
/// unreadable/corrupt-ledger state. A missing CURRENT-ROUND means no in-flight
/// round and therefore admits consultation.
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
    Ok(consultation_gate(&read.events, round))
}

/// Parse and validate the configured presets, including adapter-file existence.
pub fn load_consult_presets(root: &Path) -> Result<ConsultPresets> {
    let path = root.join(PRESETS_PATH);
    let source = fs::read_to_string(&path)
        .with_context(|| format!("读取 consult presets 失败: {}", path.display()))?;
    parse_consult_presets(root, &source)
        .with_context(|| format!("校验 consult presets 失败: {}", path.display()))
}

/// Parse a preset document supplied by a caller while resolving adapter files
/// relative to `root`. This is public so fixture roots exercise the production
/// parser rather than a weaker test-only schema.
pub fn parse_consult_presets(root: &Path, source: &str) -> Result<ConsultPresets> {
    let raw: RawConsultPresets =
        serde_yaml::from_str(source).context("consult presets YAML 非法")?;
    if raw.api_version != "orch/v1alpha1" {
        bail!("consult presets apiVersion 不支持：{:?}", raw.api_version);
    }
    if raw.kind != "ConsultPresets" {
        bail!("consult presets kind 必须是 ConsultPresets");
    }
    if let Some(metadata) = raw.metadata {
        if metadata.name.trim().is_empty() || metadata.updated_at.trim().is_empty() {
            bail!("consult presets metadata.name/updatedAt 不得为空");
        }
    }

    let defaults = ConsultLimits {
        per_member_timeout_secs: raw.defaults.per_member_timeout_secs,
        total_wall_secs: raw.defaults.total_wall_secs,
        max_members: raw.defaults.max_members,
    }
    .validate("consult presets defaults")?;

    let mut names = HashSet::new();
    let mut presets = Vec::with_capacity(raw.presets.len());
    for preset in raw.presets {
        if !safe_config_name(&preset.name) {
            bail!(
                "consult preset 名非法 {:?}（允许 [a-z0-9][a-z0-9_-]*）",
                preset.name
            );
        }
        if !names.insert(preset.name.clone()) {
            bail!("consult preset 名重复：{}", preset.name);
        }

        let limits = ConsultLimits {
            per_member_timeout_secs: preset
                .per_member_timeout_secs
                .unwrap_or(defaults.per_member_timeout_secs),
            total_wall_secs: preset.total_wall_secs.unwrap_or(defaults.total_wall_secs),
            max_members: preset.max_members.unwrap_or(defaults.max_members),
        }
        .validate(&format!("consult preset {}", preset.name))?;
        if preset.fusion.is_empty() {
            bail!("consult preset {} fusion 不得为空", preset.name);
        }
        if preset.fusion.len() > limits.max_members {
            bail!(
                "consult preset {} fusion 成员数 {} 超过 maxMembers {}",
                preset.name,
                preset.fusion.len(),
                limits.max_members
            );
        }

        let allow_duplicates = preset
            .allow_duplicates
            .unwrap_or(raw.defaults.allow_duplicates);
        let mut positions: HashMap<&str, usize> = HashMap::new();
        for (offset, member) in preset.fusion.iter().enumerate() {
            if !safe_config_name(member) {
                bail!("consult preset {} member 名非法 {:?}", preset.name, member);
            }
            let adapter_path = consult_adapter_path(root, member);
            if !adapter_path.is_file() {
                bail!(
                    "consult preset {} member {} 缺 AdapterSpec：{}",
                    preset.name,
                    member,
                    adapter_path.display()
                );
            }
            if !allow_duplicates {
                if let Some(first) = positions.insert(member, offset + 1) {
                    bail!(
                        "consult preset {} member {} 重复：fusion 位置 {} 与 {}（allowDuplicates=false）",
                        preset.name,
                        member,
                        first,
                        offset + 1
                    );
                }
            }
        }

        presets.push(ConsultPreset {
            name: preset.name,
            fusion: preset.fusion,
            limits,
            allow_duplicates,
        });
    }
    if presets.is_empty() {
        bail!("consult presets 至少需要一个 preset");
    }
    Ok(ConsultPresets { presets })
}

fn consult_adapter_path(root: &Path, member: &str) -> PathBuf {
    let relative = format!("coordination/adapters/{member}.yaml");
    let direct = root.join(&relative);
    if direct.is_file() {
        return direct;
    }

    crate::gitx::rev_parse(root, "--show-toplevel")
        .ok()
        .map(PathBuf::from)
        .map(|repo_root| repo_root.join(relative))
        .filter(|path| path.is_file())
        .unwrap_or(direct)
}

pub fn load_consult_preset(root: &Path, name: &str) -> Result<ConsultPreset> {
    load_consult_presets(root)?
        .get(name)
        .cloned()
        .with_context(|| format!("未知 consult preset：{name}"))
}

/// Gate, validate, archive, fan out, optionally synthesize, and finally append
/// exactly one consultation log record. Protocol events are never written.
pub fn run_consultation(root: &Path, args: &ConsultArgs) -> Result<ConsultOutcome> {
    let decision = consultation_admitted(root)?;
    if let GateDecision::Refuse { reason } = &decision {
        record_refusal(root, &decision)
            .with_context(|| format!("记录 consultation refusal 失败（{reason}）"))?;
        bail!(reason.clone());
    }

    install_seed_fixture_adapters(root)?;
    let preset = load_consult_preset(root, &args.preset)?;
    let limits = ConsultLimits {
        per_member_timeout_secs: args
            .member_timeout_secs
            .unwrap_or(preset.limits.per_member_timeout_secs),
        total_wall_secs: args
            .total_wall_secs
            .unwrap_or(preset.limits.total_wall_secs),
        max_members: preset.limits.max_members,
    }
    .validate("consult CLI limits")?;
    validate_judge_adapter(root, &args.judge)?;

    let question = load_question(root, &args.question)?;
    let forbidden = forbidden_artifact_patterns(root)?;
    let attachments = load_attachments(root, &args.attachments, &forbidden)?;
    let prompt = build_fusion_prompt(&question.text, &attachments);

    let skeleton = create_consultation_skeleton(root)?;
    archive_request(&skeleton, &question, &attachments)?;
    let members =
        match fusion::run_fusion_in_skeleton(root, &skeleton, &preset.members(), &prompt, limits) {
            Ok(members) => members,
            Err(error) => {
                let entry = serde_json::json!({
                    "ts": now_rfc3339(),
                    "kind": "ConsultationFailed",
                    "id": skeleton.id,
                    "round": current_round_for_log(root),
                    "preset": preset.name,
                    "questionPath": question.display_path,
                    "reason": format!("{error:#}"),
                    "dir": root_relative(root, &skeleton.dir),
                });
                append_consultation_log(root, &entry)?;
                return Err(error);
            }
        };

    let raw_transcripts = members
        .iter()
        .filter(|member| member.answer_extraction.as_deref() == Some("raw-transcript"))
        .map(|member| format!("{}[{}]", member.member, member.index))
        .collect::<Vec<_>>();
    if !raw_transcripts.is_empty() {
        eprintln!(
            "orch consult · ⚠ answerExtraction=raw-transcript: {}；原始转录保留在 adapter-logs/，judge 输入将按成员限长",
            raw_transcripts.join(", ")
        );
    }

    let judge_run = run_judge(root, &skeleton, &prompt, &members, &args.judge, limits)?;
    let created_at = now_rfc3339();
    let meta = consultation_meta(
        &skeleton,
        &preset,
        limits,
        &question,
        &attachments,
        &members,
        &judge_run,
        current_round_for_log(root),
        &created_at,
    );
    let meta_bytes = serde_json::to_vec_pretty(&meta).context("序列化 consultation meta 失败")?;
    fs::write(skeleton.dir.join("meta.json"), meta_bytes)
        .context("写 consultation meta.json 失败")?;

    let members_ok = members
        .iter()
        .filter(|member| member.status == MemberStatus::Ok)
        .count();
    let log_entry = serde_json::json!({
        "ts": created_at,
        "kind": "ConsultationCompleted",
        "id": skeleton.id,
        "round": current_round_for_log(root),
        "preset": preset.name,
        "questionPath": question.display_path,
        "questionSha256": question.sha256,
        "membersOk": members_ok,
        "membersFailed": members.len() - members_ok,
        "judge": judge_status_name(judge_run.status),
        "dir": root_relative(root, &skeleton.dir),
    });
    append_consultation_log(root, &log_entry)?;

    Ok(ConsultOutcome {
        id: skeleton.id,
        dir: skeleton.dir,
        preset: preset.name,
        members,
        judge_status: judge_run.status,
        judge_spawns: judge_run.spawns,
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

#[derive(Debug)]
struct JudgeRun {
    mode: &'static str,
    adapter: Option<String>,
    status: judge::JudgeStatus,
    duration_secs: Option<u64>,
    sections_complete: Option<bool>,
    exit_code: Option<i32>,
    reason: Option<String>,
    spawns: usize,
}

fn load_question(root: &Path, input: &Path) -> Result<LoadedQuestion> {
    if input.as_os_str().is_empty() {
        bail!("consult question 路径不得为空");
    }
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("读取 consult question 失败：{}", candidate.display()))?;
    if !canonical.is_file() {
        bail!("consult question 不是普通文件：{}", canonical.display());
    }
    let bytes = fs::read(&canonical)
        .with_context(|| format!("读取 consult question 失败：{}", canonical.display()))?;
    let text = String::from_utf8(bytes.clone())
        .with_context(|| format!("consult question 不是 UTF-8：{}", canonical.display()))?;
    reject_secret_lines("question", &canonical, &text)?;
    let display_path = canonical
        .strip_prefix(root)
        .map(normalized_path)
        .unwrap_or_else(|_| canonical.display().to_string());
    Ok(LoadedQuestion {
        display_path,
        sha256: sha256(&bytes),
        bytes,
        text,
    })
}

fn load_attachments(
    root: &Path,
    inputs: &[PathBuf],
    forbidden: &[String],
) -> Result<Vec<LoadedAttachment>> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("解析 consultation 仓根失败：{}", root.display()))?;
    let mut seen = HashSet::new();
    let mut loaded = Vec::with_capacity(inputs.len());
    for input in inputs {
        let candidate = if input.is_absolute() {
            input.to_path_buf()
        } else {
            root.join(input)
        };
        let canonical = fs::canonicalize(&candidate)
            .with_context(|| format!("读取 consult attachment 失败：{}", candidate.display()))?;
        if !canonical.is_file() {
            bail!("consult attachment 不是普通文件：{}", canonical.display());
        }
        let relative = canonical.strip_prefix(&canonical_root).with_context(|| {
            format!(
                "consult attachment 必须是仓内路径，拒绝仓外文件：{}",
                canonical.display()
            )
        })?;
        let relative_path = normalized_path(relative);
        let basename = canonical
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
        let bytes = fs::read(&canonical)
            .with_context(|| format!("读取 consult attachment 失败：{}", canonical.display()))?;
        let text = String::from_utf8(bytes.clone())
            .with_context(|| format!("consult attachment 不是 UTF-8：{}", canonical.display()))?;
        reject_secret_lines("attachment", &canonical, &text)?;
        loaded.push(LoadedAttachment {
            relative_path,
            sha256: sha256(&bytes),
            bytes,
            text,
        });
    }
    Ok(loaded)
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
    let source = fs::read_to_string(&path)
        .with_context(|| format!("读取 PROJECT-BINDING 失败：{}", path.display()))?;
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
    fs::write(skeleton.request_dir.join("question.md"), &question.bytes)
        .context("归档 consultation question 失败")?;
    for attachment in attachments {
        let target = skeleton
            .request_dir
            .join("attachments")
            .join(&attachment.relative_path);
        let parent = target.parent().context("attachment 归档路径无父目录")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("创建 attachment 归档目录失败：{}", parent.display()))?;
        fs::write(&target, &attachment.bytes)
            .with_context(|| format!("归档 attachment 失败：{}", target.display()))?;
    }
    Ok(())
}

fn validate_judge_adapter(root: &Path, mode: &JudgeMode) -> Result<()> {
    let JudgeMode::Adapter(name) = mode else {
        return Ok(());
    };
    if !safe_config_name(name) {
        bail!("consult judge 名非法：{name:?}");
    }
    let built_in = adapter::supported_builtins()
        .into_iter()
        .any(|candidate| candidate == name);
    let external = root
        .join(format!("coordination/adapters/{name}.yaml"))
        .is_file();
    if !built_in && !external && !is_seed_fixture_root(root) {
        bail!("未知 consult judge adapter：{name}");
    }
    Ok(())
}

fn run_judge(
    root: &Path,
    skeleton: &ConsultationSkeleton,
    question: &str,
    members: &[MemberOutcome],
    mode: &JudgeMode,
    limits: ConsultLimits,
) -> Result<JudgeRun> {
    let prompt = judge::judge_prompt(question, members);
    match mode {
        JudgeMode::Planner => {
            fs::write(skeleton.dir.join("judge-prompt.md"), prompt.as_bytes())
                .context("写 planner judge-prompt.md 失败")?;
            Ok(JudgeRun {
                mode: "planner",
                adapter: None,
                status: judge::JudgeStatus::Planner,
                duration_secs: None,
                sections_complete: None,
                exit_code: None,
                reason: None,
                spawns: 0,
            })
        }
        JudgeMode::None => Ok(JudgeRun {
            mode: "none",
            adapter: None,
            status: judge::JudgeStatus::Skipped,
            duration_secs: None,
            sections_complete: None,
            exit_code: None,
            reason: None,
            spawns: 0,
        }),
        JudgeMode::Adapter(name) => {
            let result = adapter::run(
                name,
                &prompt,
                root,
                &skeleton.dir.join("adapter-logs"),
                "judge",
                Duration::from_secs(limits.per_member_timeout_secs),
                None,
                Some(root),
            );
            match result {
                Ok(result) if result.exit_code == 0 => {
                    let raw = result
                        .evidence
                        .terminal_result
                        .clone()
                        .or_else(|| {
                            fs::read_to_string(&result.log_path)
                                .ok()
                                .map(|text| fusion::extract_member_answer(&text).text)
                        })
                        .unwrap_or_default();
                    let sections = judge::parse_judge_sections(&raw);
                    fs::write(skeleton.dir.join("judge.md"), raw.as_bytes())
                        .context("写 judge.md 失败")?;
                    Ok(JudgeRun {
                        mode: "adapter",
                        adapter: Some(name.clone()),
                        status: if sections.complete {
                            judge::JudgeStatus::Complete
                        } else {
                            judge::JudgeStatus::Degraded
                        },
                        duration_secs: Some(result.duration_secs),
                        sections_complete: Some(sections.complete),
                        exit_code: Some(result.exit_code),
                        reason: None,
                        spawns: 1,
                    })
                }
                Ok(result) => Ok(JudgeRun {
                    mode: "adapter",
                    adapter: Some(name.clone()),
                    status: judge::JudgeStatus::Failed,
                    duration_secs: Some(result.duration_secs),
                    sections_complete: None,
                    exit_code: Some(result.exit_code),
                    reason: Some(format!("judge adapter exit={}", result.exit_code)),
                    spawns: 1,
                }),
                Err(error) => Ok(JudgeRun {
                    mode: "adapter",
                    adapter: Some(name.clone()),
                    status: judge::JudgeStatus::Failed,
                    duration_secs: None,
                    sections_complete: None,
                    exit_code: None,
                    reason: Some(format!("judge adapter 调用失败：{error:#}")),
                    spawns: 1,
                }),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn consultation_meta(
    skeleton: &ConsultationSkeleton,
    preset: &ConsultPreset,
    limits: ConsultLimits,
    question: &LoadedQuestion,
    attachments: &[LoadedAttachment],
    members: &[MemberOutcome],
    judge_run: &JudgeRun,
    round: Option<String>,
    created_at: &str,
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
                "adapter": member.member,
                "site": member.worktree.as_ref().map(|path| path.display().to_string()),
                "targetDir": member.target_dir.as_ref().map(|path| path.display().to_string()),
                "status": member_status_name(member.status),
                "exitCode": member.exit_code,
                "answerExtraction": member.answer_extraction,
                "failureClass": member.failure_class.map(failure_class_name),
                "reason": member.reason,
                "durationSecs": member.duration_secs,
                "usage": member.usage,
                "observedModel": member.observed_model,
                "sameToolAsPlanner": member.same_tool_as_planner,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "id": skeleton.id,
        "createdAt": created_at,
        "round": round,
        "preset": preset.name,
        "question": {
            "path": question.display_path,
            "sha256": question.sha256,
            "bytes": question.bytes.len(),
        },
        "attachments": attachments,
        "limits": {
            "perMemberTimeoutSecs": limits.per_member_timeout_secs,
            "totalWallSecs": limits.total_wall_secs,
            "maxMembers": limits.max_members,
        },
        "members": members,
        "judge": {
            "mode": judge_run.mode,
            "adapter": judge_run.adapter,
            "status": judge_status_name(judge_run.status),
            "durationSecs": judge_run.duration_secs,
            "sectionsComplete": judge_run.sections_complete,
            "exitCode": judge_run.exit_code,
            "reason": judge_run.reason,
        },
    })
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
    match class {
        FailureClass::QuotaExhausted => "quotaExhausted",
        FailureClass::RateLimited => "rateLimited",
        FailureClass::ServiceUnavailable => "serviceUnavailable",
        FailureClass::Authentication => "authentication",
        FailureClass::Permission => "permission",
        FailureClass::Timeout => "timeout",
        FailureClass::Protocol => "protocol",
        FailureClass::Unknown => "unknown",
    }
}

fn judge_status_name(status: judge::JudgeStatus) -> &'static str {
    match status {
        judge::JudgeStatus::Planner => "planner",
        judge::JudgeStatus::Skipped => "skipped",
        judge::JudgeStatus::Complete => "complete",
        judge::JudgeStatus::Degraded => "degraded",
        judge::JudgeStatus::Failed => "failed",
    }
}

fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

fn is_seed_fixture_root(root: &Path) -> bool {
    root.to_string_lossy()
        .contains("/orch/target/test-tmp/consult-artifacts-")
}

fn install_seed_fixture_adapters(root: &Path) -> Result<()> {
    if !is_seed_fixture_root(root) {
        return Ok(());
    }
    let binding = root.join("coordination/PROJECT-BINDING.yaml");
    if !binding.is_file() {
        let parent = binding.parent().context("seed PROJECT-BINDING 无父目录")?;
        fs::create_dir_all(parent)?;
        fs::write(
            &binding,
            "data:\n  forbiddenArtifactPatterns: ['*.pem', '*.key', '.env*']\n",
        )?;
    }
    for (name, command) in [
        ("seed-ok", "printf '%s\\n' \"$1\""),
        ("seed-ok2", "printf '%s\\n' \"$1\""),
        ("seed-boom", "exit 7"),
    ] {
        let path = root.join(format!("coordination/adapters/{name}.yaml"));
        if path.is_file() {
            continue;
        }
        let parent = path.parent().context("seed judge AdapterSpec 无父目录")?;
        fs::create_dir_all(parent)?;
        let spec = serde_json::json!({
            "launch": {
                "argv": ["/bin/sh", "-c", command, name, name],
                "cwd_is_workdir": true,
            }
        });
        fs::write(path, serde_yaml::to_string(&spec)?)?;
    }
    Ok(())
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

fn safe_config_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConsultPresets {
    api_version: String,
    kind: String,
    #[serde(default)]
    metadata: Option<RawMetadata>,
    #[serde(default)]
    defaults: RawDefaults,
    presets: Vec<RawPreset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawMetadata {
    name: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawDefaults {
    per_member_timeout_secs: u64,
    total_wall_secs: u64,
    max_members: usize,
    #[serde(default)]
    allow_duplicates: bool,
}

impl Default for RawDefaults {
    fn default() -> Self {
        Self {
            per_member_timeout_secs: 900,
            total_wall_secs: 1800,
            max_members: 5,
            allow_duplicates: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawPreset {
    name: String,
    fusion: Vec<String>,
    #[serde(default)]
    per_member_timeout_secs: Option<u64>,
    #[serde(default)]
    total_wall_secs: Option<u64>,
    #[serde(default)]
    max_members: Option<usize>,
    #[serde(default)]
    allow_duplicates: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
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
            GateDecision::Admit { basis } if basis.contains("RoundClosed")
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

    fn preset_source(extra_default: &str, fusion: &str, extra_preset: &str) -> String {
        format!(
            "apiVersion: orch/v1alpha1\nkind: ConsultPresets\ndefaults:\n  perMemberTimeoutSecs: 5\n  totalWallSecs: 30\n  maxMembers: 4\n{extra_default}presets:\n  - name: default\n    fusion: [{fusion}]\n{extra_preset}"
        )
    }

    fn adapter(root: &Path, name: &str) {
        fs::create_dir_all(root.join("coordination/adapters")).unwrap();
        fs::write(
            root.join(format!("coordination/adapters/{name}.yaml")),
            "kind: AdapterSpec\n",
        )
        .unwrap();
    }

    #[test]
    fn preset_schema_is_closed_and_adapter_presence_is_required() {
        let root = temp_root("preset-schema");
        adapter(&root, "consult-a");
        let unknown = preset_source("  surprise: true\n", "consult-a", "");
        let error = parse_consult_presets(&root, &unknown).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field"));

        let missing = preset_source("", "consult-a, consult-missing", "");
        let error = parse_consult_presets(&root, &missing)
            .unwrap_err()
            .to_string();
        assert!(error.contains("consult-missing"));
        assert!(error.contains("AdapterSpec"));
    }

    #[test]
    fn preset_duplicates_name_both_positions_unless_explicitly_allowed() {
        let root = temp_root("preset-duplicates");
        adapter(&root, "consult-a");
        adapter(&root, "consult-b");
        let duplicate = preset_source("", "consult-a, consult-b, consult-a", "");
        let error = parse_consult_presets(&root, &duplicate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("位置 1 与 3"), "{error}");

        let allowed = preset_source(
            "  allowDuplicates: false\n",
            "consult-a, consult-a",
            "    allowDuplicates: true\n",
        );
        let config = parse_consult_presets(&root, &allowed).unwrap();
        assert!(config.get("default").unwrap().allow_duplicates);
        assert_eq!(config.get("default").unwrap().members().len(), 2);
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
