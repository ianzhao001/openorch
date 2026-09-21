//! ModeConfig -> ROUND-IR compiler and the `orch plan` host entry point.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::{binding, budget::RoundBudget, card, gitx, ledger};

pub use crate::legacy::{
    ReviewPoolCandidateV1, ReviewPoolPolicyV1, RuntimePolicyCarryForwardV1,
    RuntimePolicyResolutionV1, RuntimePolicyStateV1,
};

/// Version marker for the exact-main landed write-set guard and Rust
/// `check --all-targets` admission floor enforced by production `orch plan`.
pub const PLAN_ADMISSION_GUARDS_V1: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct TaskInput {
    pub id: String,
    pub agent: String,
    pub seed_protocol: String,
    pub has_seeds: bool,
    pub write_set: Vec<String>,
    pub frozen_paths: Vec<String>,
    pub gates_fast: Vec<String>,
    pub wall_minutes: Option<u64>,
    pub required_reviews: Vec<card::RequiredReview>,
    pub required_evidence: Vec<String>,
    pub bootstrap_pre_signoff_attempt: Option<String>,
    /// SHA-256 of the complete task-card bytes, including frontmatter and body.
    pub card_source_sha256: String,
    /// Canonical seed source path -> SHA-256 of the actual source bytes.
    pub seed_source_sha256: BTreeMap<String, String>,
}

const LEGACY_ROUND_IR_SCHEMA_VERSION: u32 = 1;
const SCHEDULED_ROUND_IR_SCHEMA_VERSION: u32 = 2;
/// Actorless task-authorization IR used by newly opened rounds.
pub const ACTORLESS_ROUND_IR_SCHEMA_VERSION: u32 = 3;

fn legacy_round_ir_schema_version() -> u32 {
    LEGACY_ROUND_IR_SCHEMA_VERSION
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundIr {
    #[serde(
        rename = "schemaVersion",
        default = "legacy_round_ir_schema_version",
        skip_serializing_if = "is_legacy_round_ir_schema_version"
    )]
    pub schema_version: u32,
    pub round: String,
    pub revision: u32,
    #[serde(rename = "modeRef", default, skip_serializing_if = "String::is_empty")]
    pub mode_ref: String,
    #[serde(default)]
    pub policy: IrPolicy,
    #[serde(default)]
    pub budgets: IrBudget,
    #[serde(default)]
    pub verification: IrVerification,
    #[serde(default)]
    pub liveness: IrLiveness,
    #[serde(default, skip_serializing_if = "IrDispatch::is_empty")]
    pub dispatch: IrDispatch,
    #[serde(default)]
    pub scheduling: IrScheduling,
    #[serde(rename = "sourceBindings", default)]
    pub source_bindings: IrSourceBindings,
    #[serde(default)]
    pub tasks: Vec<IrTask>,
    /// Deterministic topological order of task IDs consumed by run-wave and
    /// the B142 exactly-once takeover chain (B141). Same-tier members break
    /// ties by lexicographic task ID so the order is reproducible across
    /// runs. Absent on legacy persisted IRs; empty for rounds whose cards
    /// declare no `dependsOn`.
    #[serde(default)]
    pub task_order: Vec<String>,
    #[serde(default)]
    pub skipped: Vec<SkippedRule>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrSourceBindings {
    #[serde(rename = "modePath", default, skip_serializing_if = "String::is_empty")]
    pub mode_path: String,
    #[serde(rename = "modeSha256", default)]
    pub mode_sha256: String,
    #[serde(rename = "bindingSha256", default)]
    pub binding_sha256: String,
    #[serde(
        rename = "agentRegistryDigest",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub agent_registry_digest: String,
    /// SHA-256 of the harness capability registry whose terminal and receipt
    /// semantics are trusted by fallback and nongate review transitions.
    #[serde(
        rename = "harnessRegistryDigest",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub harness_registry_digest: String,
    #[serde(rename = "taskCards", default)]
    pub task_cards: BTreeMap<String, String>,
    #[serde(rename = "seedSources", default)]
    pub seed_sources: BTreeMap<String, String>,
    /// Schema-3-only in-memory sidecar. It is serialized inside each v3 task,
    /// never as an extra `sourceBindings` key, and stays empty for v1/v2.
    #[serde(skip)]
    pub(crate) v3_entry_points: BTreeMap<String, Vec<String>>,
}

/// Result of one idempotent activate/deactivate accounting transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicyTransitionOutcomeV1 {
    /// Exact policy key whose state was resolved.
    pub policy: String,
    /// State visible from the returned committed main.
    pub state: RuntimePolicyStateV1,
    /// Activation or deactivation event identity.
    pub event_id: String,
    /// Scoped one-parent accounting commit containing the event.
    pub commit_sha: String,
    /// True when the requested committed state already existed on entry.
    pub replayed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrPolicy {
    #[serde(rename = "pushPolicy")]
    pub push_policy: String,
    #[serde(rename = "mergePolicy")]
    pub merge_policy: String,
    #[serde(rename = "autoMergeOnPass")]
    pub auto_merge_on_pass: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrTask {
    pub id: String,
    pub agent: String,
    #[serde(rename = "seedProtocol")]
    pub seed_protocol: String,
    #[serde(rename = "hasSeeds", default)]
    pub has_seeds: bool,
    #[serde(rename = "writeSet")]
    pub write_set: Vec<String>,
    #[serde(rename = "frozenPaths")]
    pub frozen_paths: Vec<String>,
    #[serde(rename = "gatesFast")]
    pub gates_fast: Vec<String>,
    #[serde(rename = "wallMinutes", default)]
    pub wall_minutes: u64,
    #[serde(rename = "requiredReviews", default)]
    pub required_reviews: Vec<card::RequiredReview>,
    /// Role-keyed formal fallbacks signed beside the frozen `RequiredReview`
    /// representation, preserving historical source compatibility.
    #[serde(
        rename = "reviewFallbacks",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub review_fallbacks: Vec<card::ReviewFallback>,
    /// Explicit nongate obligations signed with the task instead of inferred
    /// from the scheduling capacity roster.
    #[serde(
        rename = "nongateSeats",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub nongate_seats: Vec<card::NongateSeat>,
    /// Optional closed quorum contract. Absence is the historical strict mode
    /// in which all formal seats remain mandatory.
    #[serde(
        rename = "reviewQuorum",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub review_quorum: Option<card::ReviewQuorumPolicy>,
    /// Signed narrow quorum exception: an exact default-primary substantive
    /// PASS may satisfy the count only after every formal seat is terminal.
    /// False is omitted so historical IR bytes retain their prior shape.
    #[serde(
        rename = "primaryPassAloneSatisfies",
        default,
        skip_serializing_if = "is_false"
    )]
    pub primary_pass_alone_satisfies: bool,
    #[serde(rename = "requiredEvidence", default)]
    pub required_evidence: Vec<String>,
    #[serde(rename = "bootstrapPreSignoffAttempt", default)]
    pub bootstrap_pre_signoff_attempt: Option<String>,
    /// B147：显式依赖边逐任务持久化（不再只有顶层 taskOrder 排序结果）。
    /// 可派发判定消费此字段；空 = 无显式前驱（与缺省等价，故省略序列化）。
    #[serde(rename = "dependsOn", default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// B147：B141 卡面 requirement（complexity/capabilities）透传入 IR，
    /// digest 覆盖——自动接替的能力闸只消费签核 IR，不回读可变卡面。
    #[serde(default, skip_serializing_if = "IrTaskRequirement::is_empty")]
    pub requirement: IrTaskRequirement,
}

/// B147 · 签核 IR 内的任务需求面（B141 卡面字段的持久化形态）。
/// 两字段全空 = 未声明需求，序列化时整体省略，旧轮 digest 不受扰。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrTaskRequirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

impl IrTaskRequirement {
    fn is_empty(&self) -> bool {
        self.complexity.is_none() && self.capabilities.is_empty()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoundIrV3Wire {
    schema_version: u32,
    round: String,
    revision: u32,
    source_bindings: IrSourceBindingsV3Wire,
    tasks: Vec<IrTaskV3Wire>,
    task_order: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IrSourceBindingsV3Wire {
    binding_sha256: String,
    task_cards: BTreeMap<String, String>,
    seed_sources: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IrTaskV3Wire {
    id: String,
    seed_protocol: String,
    has_seeds: bool,
    write_set: Vec<String>,
    frozen_paths: Vec<String>,
    entry_points: Vec<String>,
    gates_fast: Vec<String>,
    required_evidence: Vec<String>,
    depends_on: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IrSourceBindingsV3Ref<'a> {
    binding_sha256: &'a str,
    task_cards: &'a BTreeMap<String, String>,
    seed_sources: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IrTaskV3Ref<'a> {
    id: &'a str,
    seed_protocol: &'a str,
    has_seeds: bool,
    write_set: &'a [String],
    frozen_paths: &'a [String],
    entry_points: &'a [String],
    gates_fast: &'a [String],
    required_evidence: &'a [String],
    depends_on: &'a [String],
}

impl RoundIrV3Wire {
    fn into_round_ir(self) -> std::result::Result<RoundIr, String> {
        if self.schema_version != ACTORLESS_ROUND_IR_SCHEMA_VERSION {
            return Err(format!(
                "schema 3 ROUND-IR 的 schemaVersion 必须精确为 3，实得 {}",
                self.schema_version
            ));
        }
        if self.revision == 0
            || !valid_source_sha256(&self.source_bindings.binding_sha256)
            || self.tasks.is_empty()
        {
            return Err("schema 3 ROUND-IR 缺 revision/binding/tasks 基础语义".to_string());
        }
        let mut seen = BTreeSet::new();
        let mut v3_entry_points = BTreeMap::new();
        let tasks = self
            .tasks
            .into_iter()
            .map(|task| {
                if !seen.insert(task.id.clone()) {
                    return Err(format!("schema 3 ROUND-IR task id 重复: {}", task.id));
                }
                if task.entry_points.is_empty() {
                    return Err(format!("schema 3 task {} entryPoints 为空", task.id));
                }
                v3_entry_points.insert(task.id.clone(), task.entry_points);
                Ok(IrTask {
                    id: task.id,
                    agent: String::new(),
                    seed_protocol: task.seed_protocol,
                    has_seeds: task.has_seeds,
                    write_set: task.write_set,
                    frozen_paths: task.frozen_paths,
                    gates_fast: task.gates_fast,
                    wall_minutes: 0,
                    required_reviews: Vec::new(),
                    review_fallbacks: Vec::new(),
                    nongate_seats: Vec::new(),
                    review_quorum: None,
                    primary_pass_alone_satisfies: false,
                    required_evidence: task.required_evidence,
                    bootstrap_pre_signoff_attempt: None,
                    depends_on: task.depends_on,
                    requirement: IrTaskRequirement::default(),
                })
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        let task_ids = tasks
            .iter()
            .map(|task| task.id.clone())
            .collect::<BTreeSet<_>>();
        if task_ids.len() != self.task_order.len()
            || self.task_order.iter().collect::<BTreeSet<_>>().len() != self.task_order.len()
            || self.task_order.iter().any(|task| !task_ids.contains(task))
        {
            return Err("schema 3 ROUND-IR taskOrder 必须精确覆盖 tasks 且无重复".to_string());
        }
        Ok(RoundIr {
            schema_version: ACTORLESS_ROUND_IR_SCHEMA_VERSION,
            round: self.round,
            revision: self.revision,
            mode_ref: String::new(),
            policy: IrPolicy::default(),
            budgets: IrBudget::default(),
            verification: IrVerification::default(),
            liveness: IrLiveness::default(),
            dispatch: IrDispatch::default(),
            scheduling: IrScheduling::default(),
            source_bindings: IrSourceBindings {
                mode_path: String::new(),
                mode_sha256: String::new(),
                binding_sha256: self.source_bindings.binding_sha256,
                agent_registry_digest: String::new(),
                harness_registry_digest: String::new(),
                task_cards: self.source_bindings.task_cards,
                seed_sources: self.source_bindings.seed_sources,
                v3_entry_points,
            },
            tasks,
            task_order: self.task_order,
            skipped: Vec::new(),
        })
    }
}

impl Serialize for RoundIr {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION {
            let tasks = self
                .tasks
                .iter()
                .map(|task| {
                    let entry_points = self
                        .source_bindings
                        .v3_entry_points
                        .get(&task.id)
                        .ok_or_else(|| {
                            serde::ser::Error::custom(format!(
                                "schema 3 task {} 缺 entryPoints sidecar",
                                task.id
                            ))
                        })?;
                    Ok(IrTaskV3Ref {
                        id: &task.id,
                        seed_protocol: &task.seed_protocol,
                        has_seeds: task.has_seeds,
                        write_set: &task.write_set,
                        frozen_paths: &task.frozen_paths,
                        entry_points,
                        gates_fast: &task.gates_fast,
                        required_evidence: &task.required_evidence,
                        depends_on: &task.depends_on,
                    })
                })
                .collect::<std::result::Result<Vec<_>, S::Error>>()?;
            let source_bindings = IrSourceBindingsV3Ref {
                binding_sha256: &self.source_bindings.binding_sha256,
                task_cards: &self.source_bindings.task_cards,
                seed_sources: &self.source_bindings.seed_sources,
            };
            let mut state = serializer.serialize_struct("RoundIr", 6)?;
            state.serialize_field("schemaVersion", &self.schema_version)?;
            state.serialize_field("round", &self.round)?;
            state.serialize_field("revision", &self.revision)?;
            state.serialize_field("sourceBindings", &source_bindings)?;
            state.serialize_field("tasks", &tasks)?;
            state.serialize_field("taskOrder", &self.task_order)?;
            return state.end();
        }

        let mut field_count = 11;
        if self.schema_version != LEGACY_ROUND_IR_SCHEMA_VERSION {
            field_count += 1;
        }
        if !self.mode_ref.is_empty() {
            field_count += 1;
        }
        if !self.dispatch.is_empty() {
            field_count += 1;
        }
        let mut state = serializer.serialize_struct("RoundIr", field_count)?;
        if self.schema_version != LEGACY_ROUND_IR_SCHEMA_VERSION {
            state.serialize_field("schemaVersion", &self.schema_version)?;
        }
        state.serialize_field("round", &self.round)?;
        state.serialize_field("revision", &self.revision)?;
        if !self.mode_ref.is_empty() {
            state.serialize_field("modeRef", &self.mode_ref)?;
        }
        state.serialize_field("policy", &self.policy)?;
        state.serialize_field("budgets", &self.budgets)?;
        state.serialize_field("verification", &self.verification)?;
        state.serialize_field("liveness", &self.liveness)?;
        if !self.dispatch.is_empty() {
            state.serialize_field("dispatch", &self.dispatch)?;
        }
        state.serialize_field("scheduling", &self.scheduling)?;
        state.serialize_field("sourceBindings", &self.source_bindings)?;
        state.serialize_field("tasks", &self.tasks)?;
        state.serialize_field("task_order", &self.task_order)?;
        state.serialize_field("skipped", &self.skipped)?;
        state.end()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrVerification {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub adapter: String,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrLiveness {
    #[serde(rename = "monitorSeconds", default)]
    pub monitor_seconds: u64,
    #[serde(rename = "workingStallMinutes", default)]
    pub working_stall_minutes: u64,
    #[serde(rename = "confirmSamples", default)]
    pub confirm_samples: usize,
    #[serde(
        rename = "terminationGraceSeconds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub termination_grace_seconds: Option<u64>,
    #[serde(
        rename = "stallEscalationMultiplier",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stall_escalation_multiplier: Option<u64>,
    #[serde(
        rename = "autoTerminateStalled",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_terminate_stalled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrDispatch {
    #[serde(
        rename = "ackTimeoutSeconds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub ack_timeout_seconds: Option<u64>,
}

impl IrDispatch {
    fn is_empty(&self) -> bool {
        self.ack_timeout_seconds.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrScheduling {
    #[serde(rename = "allowedAgents", default)]
    pub allowed_agents: Vec<String>,
    #[serde(default)]
    pub capacities: BTreeMap<String, IrCapacity>,
    /// Agent -> quota-domain projection frozen from the validated registry.
    /// Empty legacy IRs retain the historical `domain = agent` behavior.
    #[serde(
        rename = "agentDomains",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub agent_domains: BTreeMap<String, String>,
    /// Domain-wide limits. New plans materialize singleton defaults as well as
    /// explicit shared-domain declarations so runtime never rereads YAML.
    #[serde(
        rename = "quotaDomains",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub quota_domains: BTreeMap<String, IrQuotaDomain>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrCapacity {
    #[serde(default)]
    pub agent: usize,
    #[serde(default)]
    pub quota: usize,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IrQuotaDomain {
    #[serde(default)]
    pub agent: usize,
    #[serde(default)]
    pub quota: usize,
}

impl IrScheduling {
    /// Effective per-agent runtime capacity. The historical scheduler applied
    /// both fields to the same agent load, which is exactly their minimum.
    pub fn effective_agent_capacity(&self, agent: &str) -> std::result::Result<usize, String> {
        let capacity = self
            .capacities
            .get(agent)
            .ok_or_else(|| format!("agent {agent} 缺 active ROUND-IR capacity"))?;
        let effective = capacity.agent.min(capacity.quota);
        if effective == 0 {
            return Err(format!("agent {agent} active ROUND-IR capacity 为 0"));
        }
        Ok(effective)
    }

    /// Resolve the immutable quota domain carried by the signed IR. Legacy
    /// IRs omitted this map and therefore retain `domain = agent` semantics.
    pub fn quota_domain_for<'a>(&'a self, agent: &'a str) -> std::result::Result<&'a str, String> {
        if !self.capacities.contains_key(agent) {
            return Err(format!("agent {agent} 缺 active ROUND-IR capacity"));
        }
        let domain = self
            .agent_domains
            .get(agent)
            .map(String::as_str)
            .unwrap_or(agent);
        if domain.trim().is_empty() {
            return Err(format!("agent {agent} 的 active ROUND-IR quotaDomain 为空"));
        }
        Ok(domain)
    }

    /// All signed scheduling members that consume the same quota domain.
    pub fn quota_domain_members(&self, agent: &str) -> std::result::Result<Vec<String>, String> {
        let domain = self.quota_domain_for(agent)?;
        let mut members = Vec::new();
        for candidate in &self.allowed_agents {
            if self.quota_domain_for(candidate)? == domain {
                members.push(candidate.clone());
            }
        }
        if members.is_empty() {
            return Err(format!(
                "quotaDomain {domain} 在 active ROUND-IR 中没有成员"
            ));
        }
        Ok(members)
    }

    /// Effective shared-domain capacity for one agent. New IRs read the
    /// explicit frozen map; old singleton IRs fall back to their agent limit.
    pub fn effective_domain_capacity_for_agent(
        &self,
        agent: &str,
    ) -> std::result::Result<usize, String> {
        let domain = self.quota_domain_for(agent)?;
        if let Some(capacity) = self.quota_domains.get(domain) {
            let effective = capacity.agent.min(capacity.quota);
            if effective == 0 {
                return Err(format!(
                    "quotaDomain {domain} active ROUND-IR capacity 为 0"
                ));
            }
            return Ok(effective);
        }
        let members = self.quota_domain_members(agent)?;
        if members.len() != 1 || members[0] != agent {
            return Err(format!(
                "共享 quotaDomain {domain} 缺 active ROUND-IR domain capacity"
            ));
        }
        self.effective_agent_capacity(agent)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IrBudget {
    #[serde(rename = "maxUsd", default)]
    pub max_usd: Option<f64>,
    #[serde(rename = "wallMinutes", default)]
    pub wall_minutes: Option<u64>,
    #[serde(rename = "maxModelWakes", default)]
    pub max_model_wakes: Option<u64>,
}

impl From<RoundBudget> for IrBudget {
    fn from(value: RoundBudget) -> Self {
        Self {
            max_usd: value.max_usd,
            wall_minutes: value.wall_minutes,
            max_model_wakes: value.max_model_wakes,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskValidatedPayload {
    pub ir_revision: u32,
    pub validation_digest: String,
    #[serde(default)]
    pub reverify_tasks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidationDigestClass {
    Legacy16,
    Production64,
}

pub(crate) fn classify_validation_digest(digest: &str) -> Result<ValidationDigestClass> {
    let lowercase_hex = |expected_len: usize| {
        digest.len() == expected_len
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if lowercase_hex(64) {
        Ok(ValidationDigestClass::Production64)
    } else if lowercase_hex(16) {
        Ok(ValidationDigestClass::Legacy16)
    } else {
        bail!("TaskValidated validationDigest 必须为 legacy16 或 production64 小写 hex")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanSignedOffPayload {
    pub note: String,
    pub ir_revision: u32,
    pub validation_digest: String,
}

impl PlanSignedOffPayload {
    fn validate(&self) -> Result<()> {
        if self.note.trim().is_empty() {
            bail!("PlanSignedOff note 不得为空白");
        }
        if self.ir_revision == 0 {
            bail!("PlanSignedOff irRevision 必须大于 0");
        }
        if !valid_source_sha256(&self.validation_digest) {
            bail!("PlanSignedOff validationDigest 必须为 64 位小写 hex");
        }
        Ok(())
    }
}

pub fn plan_signed_off_payload(
    note: &str,
    revision: u32,
    digest: &str,
) -> Result<serde_json::Value> {
    let payload = PlanSignedOffPayload {
        note: note.to_string(),
        ir_revision: revision,
        validation_digest: digest.to_string(),
    };
    payload.validate()?;
    Ok(serde_json::to_value(payload)?)
}

/// Decode only a production user sign-off. Historical note-only events are
/// deliberately ignored; once either binding key is present the complete
/// typed schema is mandatory and malformed/extra fields fail closed.
pub fn decode_user_plan_signoff(
    event: &EventRecord,
    round: &str,
) -> Result<Option<PlanSignedOffPayload>> {
    if event.kind != "PlanSignedOff"
        || event.actor != "user"
        || event.task_id.is_some()
        || event.round.as_deref() != Some(round)
    {
        return Ok(None);
    }
    let Some(object) = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
    else {
        return Ok(None);
    };
    if !object.contains_key("irRevision") && !object.contains_key("validationDigest") {
        return Ok(None);
    }
    let payload: PlanSignedOffPayload =
        serde_json::from_value(event.payload.clone().context("PlanSignedOff 缺 payload")?)
            .with_context(|| format!("PlanSignedOff {} payload 非 canonical", event.event_id))?;
    payload.validate()?;
    Ok(Some(payload))
}

pub fn matching_user_plan_signoff_positions(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<Vec<usize>> {
    let mut positions = Vec::new();
    for (position, event) in events.iter().enumerate() {
        let Some(payload) = decode_user_plan_signoff(event, round)? else {
            continue;
        };
        if payload.ir_revision == revision && payload.validation_digest == digest {
            positions.push(position);
        }
    }
    Ok(positions)
}

pub fn task_validated_payload(revision: u32, digest: &str) -> serde_json::Value {
    task_validated_payload_with_reverify(revision, digest, &[])
}

pub fn task_validated_payload_with_reverify(
    revision: u32,
    digest: &str,
    reverify_tasks: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "irRevision": revision,
        "validationDigest": digest,
        "reverifyTasks": reverify_tasks,
    })
}

pub fn decode_task_validated(event: &EventRecord) -> Result<TaskValidatedPayload> {
    if event.kind != "TaskValidated" {
        bail!("事件 {} 不是 TaskValidated", event.event_id);
    }
    let payload = event
        .payload
        .clone()
        .with_context(|| format!("TaskValidated {} 缺 payload", event.event_id))?;
    let decoded: TaskValidatedPayload = serde_json::from_value(payload).with_context(|| {
        format!(
            "TaskValidated {} payload 非 canonical schema",
            event.event_id
        )
    })?;
    if decoded.ir_revision == 0 {
        bail!("TaskValidated {} irRevision 必须大于 0", event.event_id);
    }
    if decoded.validation_digest.is_empty() {
        bail!("TaskValidated {} validationDigest 为空", event.event_id);
    }
    if decoded
        .reverify_tasks
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
        || decoded
            .reverify_tasks
            .iter()
            .any(|task_id| !safe_identity_component(task_id))
    {
        bail!(
            "TaskValidated {} reverifyTasks 必须是排序、去重的安全 taskId",
            event.event_id
        );
    }
    Ok(decoded)
}

/// Decode a production TaskValidated event only after its complete envelope is
/// proven to be runtime-owned and round-scoped (never task-scoped).  The
/// payload-only codec above remains useful for contract tests and migrations;
/// authorization paths must use this stricter entry point.
pub fn decode_runtime_task_validated(
    event: &EventRecord,
    round: &str,
) -> Result<TaskValidatedPayload> {
    if event.kind != "TaskValidated"
        || event.actor != "runtime:orch"
        || event.task_id.is_some()
        || event.round.as_deref() != Some(round)
    {
        bail!(
            "TaskValidated {} envelope 非 canonical runtime/round tuple",
            event.event_id
        );
    }
    let decoded = decode_task_validated(event)?;
    if !valid_source_sha256(&decoded.validation_digest) {
        bail!(
            "TaskValidated {} production validationDigest 必须为 64 位小写 hex",
            event.event_id
        );
    }
    Ok(decoded)
}

fn validation_high_water(
    events: &[EventRecord],
    round: &str,
) -> Result<Option<(usize, TaskValidatedPayload, bool)>> {
    let mut high: Option<(usize, TaskValidatedPayload, bool)> = None;
    let mut seen_production = false;
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "TaskValidated"
            && event.actor == "runtime:orch"
            && event.task_id.is_none()
            && event.round.as_deref() == Some(round)
    }) {
        // Historical producer payloads that are not even the canonical schema
        // are ignored for migration.  Canonical-schema short digests still
        // reserve their revision as a legacy high-water marker but never
        // authorize production.
        let payload = decode_task_validated(event)?;
        let class = classify_validation_digest(&payload.validation_digest)?;
        let production_digest = matches!(class, ValidationDigestClass::Production64);
        if seen_production && matches!(class, ValidationDigestClass::Legacy16) {
            bail!("TaskValidated 禁止在 production validation 后降级为 legacy16");
        }
        seen_production |= production_digest;
        if let Some((_, previous, _)) = &high {
            if payload.ir_revision < previous.ir_revision {
                bail!("TaskValidated revision 非单调递增");
            }
            if payload.ir_revision == previous.ir_revision
                && payload.validation_digest != previous.validation_digest
            {
                bail!("同一 TaskValidated revision 出现多个 validationDigest");
            }
        }
        high = Some((position, payload, production_digest));
    }
    Ok(high)
}

pub fn matching_plan_signoff(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<bool> {
    for event in events
        .iter()
        .filter(|event| event.kind == "PlanSignedOff" && event.round.as_deref() == Some(round))
    {
        let Some(payload) = &event.payload else {
            continue;
        };
        let Some(object) = payload.as_object() else {
            bail!("PlanSignedOff {} payload 非 object", event.event_id);
        };
        let revision_value = object.get("irRevision");
        let digest_value = object.get("validationDigest");
        if revision_value.is_none() && digest_value.is_none() {
            continue;
        }
        let Some(signed_revision) = revision_value.and_then(serde_json::Value::as_u64) else {
            bail!("PlanSignedOff {} irRevision 类型错误", event.event_id);
        };
        let Some(signed_digest) = digest_value.and_then(serde_json::Value::as_str) else {
            bail!("PlanSignedOff {} validationDigest 类型错误", event.event_id);
        };
        if signed_revision == revision as u64 && signed_digest == digest {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Production sign-off matcher.  Events with the right payload but a
/// non-user actor or task-scoped envelope are irrelevant and cannot either
/// authorize a round or suppress a later legitimate user sign-off.
pub fn matching_user_plan_signoff(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<bool> {
    Ok(matching_user_plan_signoff_position(events, round, revision, digest)?.is_some())
}

pub fn matching_user_plan_signoff_position(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<Option<usize>> {
    let validation_position = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "TaskValidated"
                && event.actor == "runtime:orch"
                && event.task_id.is_none()
                && event.round.as_deref() == Some(round)
        })
        .filter_map(|(position, event)| {
            decode_runtime_task_validated(event, round)
                .ok()
                .filter(|payload| {
                    payload.ir_revision == revision && payload.validation_digest == digest
                })
                .map(|_| position)
        })
        .last();
    let Some(validation_position) = validation_position else {
        return Ok(None);
    };
    Ok(
        matching_user_plan_signoff_positions(events, round, revision, digest)?
            .into_iter()
            .filter(|position| *position > validation_position)
            .last(),
    )
}

fn validation_event_position_and_payload(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<Option<(usize, TaskValidatedPayload)>> {
    let mut matched = None;
    for (position, event) in events.iter().enumerate() {
        if event.kind != "TaskValidated"
            || event.actor != "runtime:orch"
            || event.task_id.is_some()
            || event.round.as_deref() != Some(round)
        {
            continue;
        }
        let payload = decode_task_validated(event)?;
        if !valid_source_sha256(&payload.validation_digest) {
            continue;
        }
        if payload.ir_revision == revision && payload.validation_digest == digest {
            matched = Some((position, payload));
        }
    }
    Ok(matched)
}

pub fn reverify_tasks_for_validation(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<Vec<String>> {
    Ok(
        validation_event_position_and_payload(events, round, revision, digest)?
            .map(|(_, payload)| payload.reverify_tasks)
            .unwrap_or_default(),
    )
}

/// Return validation-bound reverify tasks that still lack a fresh, planner
/// owned SeedOracleVerified event after the matching TaskValidated event.
pub fn pending_reverify_tasks(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    digest: &str,
) -> Result<Vec<String>> {
    let Some((validation_position, validation)) =
        validation_event_position_and_payload(events, round, revision, digest)?
    else {
        return Ok(Vec::new());
    };
    let mut pending = Vec::new();
    for task_id in validation.reverify_tasks {
        let verified = events
            .iter()
            .skip(validation_position + 1)
            .filter(|event| {
                event.kind == "SeedOracleVerified"
                    && event.actor == "planner"
                    && event.task_id.as_deref() == Some(task_id.as_str())
                    && event.round.as_deref() == Some(round)
            })
            .try_fold(false, |found, event| -> Result<bool> {
                let Some(payload) = event.payload.as_ref() else {
                    return Ok(found);
                };
                let Some(value) = payload.get("irRevision") else {
                    return Ok(found);
                };
                let bound_revision = value.as_u64().with_context(|| {
                    format!(
                        "SeedOracleVerified {} payload.irRevision 类型错误",
                        event.event_id
                    )
                })?;
                Ok(found || bound_revision == revision as u64)
            })?;
        if !verified {
            pending.push(task_id);
        }
    }
    Ok(pending)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedRule {
    pub rule: String,
    pub reason: String,
}

#[derive(Debug)]
pub struct PlanOutcome {
    pub ir_path: PathBuf,
    pub revision: u32,
    pub digest: String,
    pub skipped: Vec<SkippedRule>,
    pub ir_written: bool,
    pub event_appended: bool,
    pub reverify_tasks: Vec<String>,
}

/// Read-only reconstruction of a retired schema 1/2 plan artifact.
///
/// The production crate can parse and reproduce historical bytes but exposes
/// no function that writes those bytes or appends their validation event.
#[derive(Debug)]
pub struct LegacyPlanArtifact {
    pub outcome: PlanOutcome,
    pub ir_bytes: Option<Vec<u8>>,
    pub validation_event: Option<EventRecord>,
}

#[derive(Debug)]
pub struct ReadonlyIrValidation {
    pub persisted_revision: u32,
    pub persisted_digest: String,
    pub digest_matches: bool,
    pub candidate: RoundIr,
}

// RoundBudget is owned by this crate, so the plan module can supply the
// serialization half needed by ROUND-IR without changing the frozen module.
impl Serialize for RoundBudget {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RoundBudget", 3)?;
        state.serialize_field("maxUsd", &self.max_usd)?;
        state.serialize_field("wallMinutes", &self.wall_minutes)?;
        state.serialize_field("maxModelWakes", &self.max_model_wakes)?;
        state.end()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ModeConfig {
    #[serde(default)]
    agents: BTreeMap<String, ModeAgent>,
    #[serde(default)]
    hitl: ModeHitl,
    #[serde(default)]
    budgets: ModeBudgets,
    #[serde(default)]
    git: ModeGit,
    #[serde(default)]
    verification: ModeVerification,
    #[serde(default)]
    liveness: ModeLiveness,
    #[serde(default)]
    dispatch: ModeDispatch,
    #[serde(default)]
    scheduling: ModeScheduling,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeAgent {
    #[serde(default)]
    tier: Option<String>,
    #[serde(rename = "sameAs", default)]
    same_as: Option<String>,
    #[serde(default)]
    adapter: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeVerification {
    #[serde(default)]
    mode: String,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeLiveness {
    #[serde(rename = "monitorSeconds", default)]
    monitor_seconds: u64,
    #[serde(rename = "workingStallMinutes", default)]
    working_stall_minutes: u64,
    #[serde(rename = "confirmSamples", default)]
    confirm_samples: usize,
    #[serde(rename = "terminationGraceSeconds", default)]
    termination_grace_seconds: Option<u64>,
    #[serde(rename = "stallEscalationMultiplier", default)]
    stall_escalation_multiplier: Option<u64>,
    #[serde(rename = "autoTerminateStalled", default)]
    auto_terminate_stalled: Option<bool>,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeDispatch {
    #[serde(rename = "ackTimeoutSeconds", default)]
    ack_timeout_seconds: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeScheduling {
    #[serde(rename = "allowedAgents", default)]
    allowed_agents: Vec<String>,
    #[serde(default)]
    capacities: BTreeMap<String, IrCapacity>,
    #[serde(
        rename = "quotaDomains",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    quota_domains: BTreeMap<String, IrQuotaDomain>,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeHitl {
    #[serde(rename = "mergeGate", default)]
    merge_gate: String,
}

#[derive(Default, Serialize, Deserialize)]
struct ModeBudgets {
    #[serde(default)]
    round: RoundBudget,
}

#[derive(Serialize, Deserialize)]
struct ModeGit {
    #[serde(rename = "pushPolicy", default = "forbidden")]
    push_policy: String,
    #[serde(rename = "mergePolicy", default = "default_merge_policy")]
    merge_policy: String,
}

/// Bind a signed round to one named ModeConfig.  Absence is deliberately an
/// error: callers that support the legacy single-mode layout must make that
/// compatibility decision explicitly, before invoking this kernel.
pub fn mode_ref_binding(
    mode_ref: Option<&str>,
    available: &[String],
) -> std::result::Result<String, String> {
    let mode_ref = mode_ref
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "modeRef 缺失；拒绝按文件名顺序猜测 ModeConfig".to_string())?;
    if available.iter().any(|candidate| candidate == mode_ref) {
        Ok(mode_ref.to_string())
    } else {
        Err(format!(
            "modeRef {mode_ref:?} 不在可用 ModeConfig 集合中: {}",
            available.join(", ")
        ))
    }
}

/// A signed scheduling hierarchy is data, not a hardcoded provider order.
/// It is valid exactly when it is non-empty, registered, and duplicate-free.
pub fn hierarchy_structurally_valid(
    agents: &[String],
    registered: &[String],
) -> std::result::Result<(), String> {
    if agents.is_empty() {
        return Err("scheduling.allowedAgents 不得为空".into());
    }
    let registered = registered
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    for agent in agents {
        if !registered.contains(agent.as_str()) {
            return Err(format!("scheduling.allowedAgents 含未登记 agent: {agent}"));
        }
        if !seen.insert(agent.as_str()) {
            return Err(format!("scheduling.allowedAgents 含重复 agent: {agent}"));
        }
    }
    Ok(())
}

impl Default for ModeGit {
    fn default() -> Self {
        Self {
            push_policy: forbidden(),
            merge_policy: default_merge_policy(),
        }
    }
}

#[derive(Default, Deserialize)]
struct PlanBinding {
    #[serde(default)]
    workspace: PlanWorkspace,
    #[serde(default)]
    scope: PlanScope,
    #[serde(default)]
    git: PlanGit,
    #[serde(default)]
    verification: PlanVerification,
}

#[derive(Deserialize)]
struct PlanWorkspace {
    #[serde(rename = "worktreeRoot", default = "default_worktree_root")]
    worktree_root: String,
}

impl Default for PlanWorkspace {
    fn default() -> Self {
        Self {
            worktree_root: default_worktree_root(),
        }
    }
}

#[derive(Default, Deserialize)]
struct PlanScope {
    #[serde(rename = "protectedPaths", default)]
    protected_paths: Vec<String>,
}

#[derive(Deserialize)]
struct PlanGit {
    #[serde(rename = "pushPolicy", default = "forbidden")]
    push_policy: String,
}

impl Default for PlanGit {
    fn default() -> Self {
        Self {
            push_policy: forbidden(),
        }
    }
}

#[derive(Default, Deserialize)]
struct PlanVerification {
    #[serde(rename = "independentVerifier", default)]
    independent_verifier: String,
}

fn forbidden() -> String {
    "forbidden".into()
}

fn default_merge_policy() -> String {
    "ff-only-else-no-ff".into()
}

fn default_worktree_root() -> String {
    ".worktrees".into()
}

fn has_tier_f(mode: &ModeConfig) -> bool {
    mode.agents.values().any(|agent| {
        agent
            .tier
            .as_deref()
            .is_some_and(|tier| tier.eq_ignore_ascii_case("F"))
    })
}

fn source_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Content-address the complete agent registry input: `agents.yaml` plus
/// every `coordination/tools/*.yaml` file, in canonical relative-path order.
/// Absolute roots never enter the hash, so identical registries at different
/// fixture paths produce the same digest.
pub fn agent_registry_digest(root: &Path) -> Result<String> {
    let mut sources = vec![(
        "coordination/agents.yaml".to_string(),
        source_file_bytes(
            root,
            "coordination/agents.yaml",
            "AgentRegistry digest source",
        )?,
    )];
    let tools_dir = root.join("coordination/tools");
    match fs::symlink_metadata(&tools_dir) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                bail!(
                    "AgentRegistry tools source 必须是真实目录: {}",
                    tools_dir.display()
                );
            }
            let entries = fs::read_dir(&tools_dir).with_context(|| {
                format!("读取 AgentRegistry tools 失败: {}", tools_dir.display())
            })?;
            for entry in entries {
                let entry = entry.context("枚举 AgentRegistry tools 失败")?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("AgentRegistry tool 文件名必须为 UTF-8"))?;
                if !name.ends_with(".yaml") {
                    continue;
                }
                let rel = format!("coordination/tools/{name}");
                let bytes = source_file_bytes(root, &rel, "ToolDefinition digest source")?;
                sources.push((rel, bytes));
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("检查 AgentRegistry tools 失败: {}", tools_dir.display()))
        }
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));

    let mut digest = Sha256::new();
    digest.update(b"orch-agent-registry-v1\0");
    for (rel, bytes) in sources {
        digest.update((rel.len() as u64).to_be_bytes());
        digest.update(rel.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    Ok(hex::encode(digest.finalize()))
}

/// Fold a signed registry genesis digest through audited amendments in ledger
/// order.  Every record must start at the previous record's exact output;
/// missing, duplicated, or reordered links fail closed instead of weakening
/// the signed registry binding.
pub fn expected_registry_digest_with_amendments(
    genesis: &str,
    amendments: &[crate::registry::AgentPinAmendment],
) -> Result<String> {
    crate::registry::fold_agent_pin_amendments(genesis, amendments)
}

/// Cross-check the round scheduling envelope against the loaded registry.
/// The registry is an upper bound: a round may narrow roles/capacities, but it
/// cannot invent an agent, grant a new role, or omit a shared tool domain.
pub fn validate_scheduling_against_registry(
    scheduling: &IrScheduling,
    quota_domains: &BTreeMap<String, IrQuotaDomain>,
    definitions: &BTreeMap<String, crate::registry::AgentDefinition>,
) -> std::result::Result<(), String> {
    if scheduling.allowed_agents.is_empty() {
        return Err("scheduling.allowedAgents 不得为空".into());
    }
    let mut seen = BTreeSet::new();
    for agent in &scheduling.allowed_agents {
        if !seen.insert(agent.as_str()) {
            return Err(format!("scheduling.allowedAgents 含重复 agent: {agent}"));
        }
        let definition = definitions
            .get(agent)
            .ok_or_else(|| format!("scheduling.allowedAgents 含注册表不存在的 agent: {agent}"))?;
        let capacity = scheduling
            .capacities
            .get(agent)
            .ok_or_else(|| format!("scheduling.capacities 缺 agent: {agent}"))?;
        if capacity.agent == 0 || capacity.quota == 0 || capacity.roles.is_empty() {
            return Err(format!(
                "scheduling.capacities.{agent} 必须显式给出非零 agent/quota/roles"
            ));
        }

        // Headerless legacy registry entries predate registry-owned roles.
        // Preserve their historical behavior; typed entries (or any entry
        // that declares roles) make the registry a strict qualification cap.
        if definition.tool.is_some() || !definition.roles.is_empty() {
            let registered = definition
                .roles
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            for role in &capacity.roles {
                if !registered.contains(role.as_str()) {
                    return Err(format!(
                        "scheduling.capacities.{agent}.roles 含超出注册表资格上界的 role: {role}"
                    ));
                }
            }
        }
    }
    if scheduling.capacities.len() != scheduling.allowed_agents.len()
        || scheduling
            .capacities
            .keys()
            .any(|agent| !seen.contains(agent.as_str()))
    {
        return Err("scheduling.capacities 必须精确覆盖 allowedAgents".into());
    }

    let mut members_by_domain = BTreeMap::<String, Vec<String>>::new();
    for agent in &scheduling.allowed_agents {
        let domain = definitions[agent].profile.quota_domain.trim();
        if domain.is_empty() {
            return Err(format!("注册表 agent {agent} 的 quotaDomain 为空"));
        }
        members_by_domain
            .entry(domain.to_string())
            .or_default()
            .push(agent.clone());
    }

    for (domain, members) in &members_by_domain {
        if members.len() > 1 && !quota_domains.contains_key(domain) {
            return Err(format!(
                "共享 quotaDomain {domain} 有多个实例 [{}]，必须显式声明域上限",
                members.join(", ")
            ));
        }
    }
    for (domain, domain_capacity) in quota_domains {
        if domain.trim().is_empty() || domain.trim() != domain {
            return Err(format!("quotaDomains 含非法域名: {domain:?}"));
        }
        if domain_capacity.agent == 0 || domain_capacity.quota == 0 {
            return Err(format!(
                "quotaDomains.{domain} 必须显式给出非零 agent/quota"
            ));
        }
        let members = members_by_domain
            .get(domain)
            .ok_or_else(|| format!("quotaDomains.{domain} 没有 allowedAgents 成员"))?;
        for member in members {
            let member_capacity = &scheduling.capacities[member];
            if domain_capacity.agent < member_capacity.agent
                || domain_capacity.quota < member_capacity.quota
            {
                return Err(format!(
                    "quotaDomains.{domain} 上限低于成员 {member}：domain={}/{} member={}/{}",
                    domain_capacity.agent,
                    domain_capacity.quota,
                    member_capacity.agent,
                    member_capacity.quota
                ));
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct AgentRegistryMarker {
    #[serde(rename = "apiVersion", default)]
    api_version: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// Freeze registry-derived domains and its content digest into a candidate IR.
/// Strict v1 registries bind even when every entry still uses the legacy wake
/// shape; headerless archived registries retain the pre-B234 compatibility path.
fn bind_agent_registry_projection(root: &Path, ir: &mut RoundIr) -> Result<()> {
    let registry_path = root.join("coordination/agents.yaml");
    match fs::symlink_metadata(&registry_path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!("AgentRegistry 必须是 regular non-symlink file"),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("检查 AgentRegistry 失败"),
    }

    let digest_before = agent_registry_digest(root)?;
    let registry_bytes = source_file_bytes(root, "coordination/agents.yaml", "AgentRegistry")?;
    let marker: AgentRegistryMarker =
        serde_yaml::from_slice(&registry_bytes).context("解析 AgentRegistry header 失败")?;
    let strict_registry = marker.api_version.as_deref() == Some("orch/v1alpha1")
        && marker.kind.as_deref() == Some("AgentRegistry")
        || !ir.scheduling.quota_domains.is_empty();
    if !strict_registry {
        return Ok(());
    }
    let definitions = crate::registry::load_agent_definitions(root)?;

    let declared_domains = ir.scheduling.quota_domains.clone();
    validate_scheduling_against_registry(&ir.scheduling, &declared_domains, &definitions)
        .map_err(anyhow::Error::msg)?;

    let mut agent_domains = BTreeMap::new();
    let mut members_by_domain = BTreeMap::<String, Vec<String>>::new();
    for agent in &ir.scheduling.allowed_agents {
        let domain = definitions[agent].profile.quota_domain.clone();
        agent_domains.insert(agent.clone(), domain.clone());
        members_by_domain
            .entry(domain)
            .or_default()
            .push(agent.clone());
    }
    let mut frozen_domains = declared_domains;
    for (domain, members) in members_by_domain {
        if frozen_domains.contains_key(&domain) {
            continue;
        }
        let member = members
            .first()
            .context("validated quotaDomain unexpectedly has no member")?;
        let capacity = &ir.scheduling.capacities[member];
        frozen_domains.insert(
            domain,
            IrQuotaDomain {
                agent: capacity.agent,
                quota: capacity.quota,
            },
        );
    }
    let digest_after = agent_registry_digest(root)?;
    if digest_before != digest_after {
        bail!("AgentRegistry 在 plan 读取期间发生漂移");
    }
    ir.scheduling.agent_domains = agent_domains;
    ir.scheduling.quota_domains = frozen_domains;
    ir.source_bindings.agent_registry_digest = digest_after;
    Ok(())
}

/// SHA-256 of the canonical ModeConfig contract projection. Runtime budgets
/// are deliberately removed after parsing; comments, whitespace, and budget
/// changes therefore cannot invalidate an already-authorized attempt.
pub fn contract_digest_of_mode(mode_yaml: &str) -> Result<String> {
    let mode: ModeConfig = serde_yaml::from_str(mode_yaml).context("解析 ModeConfig 失败")?;
    let mut projection =
        serde_json::to_value(mode).context("规范化 ModeConfig contract projection 失败")?;
    projection
        .as_object_mut()
        .context("ModeConfig contract projection 非 object")?
        .remove("budgets");
    // ACP enables serde_json::preserve_order in workspace builds. Preserve the
    // established BTreeMap ordering for every nested object before signing.
    projection.sort_all_objects();
    let normalized =
        serde_json::to_vec(&projection).context("序列化 ModeConfig contract projection 失败")?;
    Ok(source_sha256(&normalized))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightAttempt {
    pub task_id: String,
    pub attempt_id: String,
    pub agent: String,
    pub state: String,
}

impl InflightAttempt {
    /// Compatibility helper for compact contract tests and human-facing
    /// diagnostics: all four required identity/state fields are searchable.
    pub fn contains(&self, needle: &str) -> bool {
        self.task_id.contains(needle)
            || self.attempt_id.contains(needle)
            || self.agent.contains(needle)
            || self.state.contains(needle)
    }
}

fn parse_ledger_text(ledger: &str) -> Result<Vec<EventRecord>> {
    ledger
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            (!line.is_empty()).then_some((index, line))
        })
        .map(|(index, line)| {
            serde_json::from_str::<EventRecord>(line)
                .with_context(|| format!("账本坏行 #{}", index + 1))
        })
        .collect()
}

fn payload_string<'a>(event: &'a EventRecord, key: &str) -> Result<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{} {} 缺非空 {key}", event.kind, event.event_id))
}

fn replan_invalidation_from_events(
    events: &[EventRecord],
    round: &str,
) -> Result<Vec<InflightAttempt>> {
    let terminal_kinds = [
        "AttemptBlocked",
        "AttemptCrashed",
        "AttemptTimedOut",
        "AttemptFailed",
    ];
    let mut live: BTreeMap<(String, String), InflightAttempt> = BTreeMap::new();

    for event in events
        .iter()
        .filter(|event| event.round.as_deref() == Some(round))
    {
        let Some(task_id) = event.task_id.as_deref() else {
            if event.kind == "DispatchIssued" {
                bail!("DispatchIssued {} 缺 taskId", event.event_id);
            }
            continue;
        };
        if event.kind == "DispatchIssued" {
            let attempt_id = payload_string(event, "attemptId")?;
            let agent = payload_string(event, "agent")?;
            live.insert(
                (task_id.to_string(), attempt_id.to_string()),
                InflightAttempt {
                    task_id: task_id.to_string(),
                    attempt_id: attempt_id.to_string(),
                    agent: agent.to_string(),
                    state: "DispatchIssued".to_string(),
                },
            );
            continue;
        }
        if event.kind == "TaskRecorded" {
            live.retain(|(task, _), _| task != task_id);
            continue;
        }
        // H29：屏障释放 = 本轮已明示该 attempt 不会 Recorded（合后门红且补记门
        // 结构性不可能转绿）。它的授权链已经用不掉了，replan 作废它零代价；
        // 若仍算「在飞」，守卫会把释放后的轮永久锁死在无法 replan 也无法收轮的
        // 状态——恢复出口必须真的能恢复。
        if event.kind == "EscalationRaised"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str)
                == Some("post-merge-gate-released")
        {
            live.retain(|(task, _), _| task != task_id);
            continue;
        }
        if terminal_kinds.contains(&event.kind.as_str()) {
            let attempt_id = payload_string(event, "attemptId")?;
            live.remove(&(task_id.to_string(), attempt_id.to_string()));
            continue;
        }
        if event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && crate::attempt::verdict_is_takeover_terminal(event.payload.as_ref())
        {
            let attempt_id = payload_string(event, "attemptId")?;
            live.remove(&(task_id.to_string(), attempt_id.to_string()));
            continue;
        }
        if let Some(attempt_id) = event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("attemptId"))
            .and_then(serde_json::Value::as_str)
        {
            if let Some(attempt) = live.get_mut(&(task_id.to_string(), attempt_id.to_string())) {
                attempt.state = event.kind.clone();
            }
        }
    }
    Ok(live.into_values().collect())
}

/// List dispatched attempts that have not reached a terminal attempt/task
/// event. Malformed JSON or malformed dispatch/terminal identity fails closed.
pub fn replan_invalidation_report(ledger: &str, round: &str) -> Result<Vec<InflightAttempt>> {
    replan_invalidation_from_events(&parse_ledger_text(ledger)?, round)
}

fn ensure_replan_safe(contract_changed: bool, events: &[EventRecord], round: &str) -> Result<()> {
    if !contract_changed {
        return Ok(());
    }
    let in_flight = replan_invalidation_from_events(events, round)?;
    if in_flight.is_empty() {
        return Ok(());
    }
    let detail = in_flight
        .iter()
        .map(|attempt| {
            format!(
                "taskId={} attemptId={} agent={} state={}",
                attempt.task_id, attempt.attempt_id, attempt.agent, attempt.state
            )
        })
        .collect::<Vec<_>>()
        .join("\n- ");
    bail!(
        "replan 会作废在飞 attempt，已拒绝:\n- {detail}\n\
         请先把这些 attempt 驱动到终态（必要时写 BLOCKED 并顶替）再 replan"
    )
}

fn dispatched_tasks_from_events(events: &[EventRecord], round: &str) -> Result<BTreeSet<String>> {
    let mut dispatched = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event.round.as_deref() == Some(round) && event.kind == "DispatchIssued")
    {
        let task_id = event
            .task_id
            .as_deref()
            .with_context(|| format!("DispatchIssued {} 缺 taskId", event.event_id))?;
        payload_string(event, "attemptId")?;
        payload_string(event, "agent")?;
        dispatched.insert(task_id.to_string());
    }
    Ok(dispatched)
}

/// Whether current scheduling policy applies to a task. Once a task has any
/// DispatchIssued event, the policy checked at dispatch remains its contract.
pub fn scheduling_rules_apply(ledger: &str, round: &str, task_id: &str) -> Result<bool> {
    let events = parse_ledger_text(ledger)?;
    Ok(!dispatched_tasks_from_events(&events, round)?.contains(task_id))
}

fn round_number_at_least(round: &str, minimum: u64) -> bool {
    round
        .strip_prefix('r')
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|number| number >= minimum)
}

pub(crate) fn valid_source_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Identity components used in signed IR and canonical artifact paths share
/// one grammar so plan-accepted contracts cannot become unclosable later.
pub fn safe_identity_component(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Require every seed delivery target to be authorized by the card writeSet.
///
/// Both inputs use the task-card path grammar: exact paths or a trailing
/// `/**` prefix glob.  The error names every uncovered target so `orch plan`
/// can reject a bad card before it creates a revision.
pub fn seed_targets_covered_by_write_set(
    seed_targets: &[String],
    write_set: &[String],
) -> Result<()> {
    let uncovered = seed_targets
        .iter()
        .filter(|target| !card::path_matches(write_set, target))
        .cloned()
        .collect::<Vec<_>>();
    if !uncovered.is_empty() {
        bail!("seed target 不在 writeSet: {}", uncovered.join(", "));
    }
    Ok(())
}

/// Prove that a card's write authorization is disjoint from its frozen paths.
///
/// Intersection is symmetric: an exact write under a frozen `dir/**`, and a
/// broad write `dir/**` containing an exact frozen path, are both rejected.
pub fn write_set_disjoint_from_frozen(write_set: &[String], frozen: &[String]) -> Result<()> {
    let mut conflicts = Vec::new();
    for writable in write_set {
        for frozen_path in frozen {
            if card::path_matches(std::slice::from_ref(frozen_path), writable)
                || card::path_matches(std::slice::from_ref(writable), frozen_path)
            {
                conflicts.push(format!("{writable} ↔ {frozen_path}"));
            }
        }
    }
    conflicts.sort();
    conflicts.dedup();
    if !conflicts.is_empty() {
        bail!("writeSet 命中 frozenPaths: {}", conflicts.join(", "));
    }
    Ok(())
}

/// Pure input for the crate-root module reachability guard.
///
/// `head_files` and `lib_declarations` are explicit snapshots so the contract
/// can be tested without consulting a mutable working tree. Production builds
/// the snapshot exclusively from `git show HEAD:<path>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleReachabilityInput {
    pub task_id: String,
    pub write_set: Vec<String>,
    pub head_files: BTreeSet<String>,
    pub lib_declarations: BTreeMap<String, Vec<String>>,
}

/// Refuse new, one-level library modules that the card cannot make reachable.
///
/// Cargo discovers integration tests, `build.rs`, and `src/main.rs` without a
/// `lib.rs` declaration, so only exact
/// `orch/crates/<crate>/src/<module>.rs` entries are candidates.
pub fn module_reachability_error(input: &ModuleReachabilityInput) -> Option<String> {
    let mut unreachable = Vec::new();
    for module_path in &input.write_set {
        let Some((lib_path, module_name)) = crate_root_module(module_path) else {
            continue;
        };
        if input.head_files.contains(module_path)
            || card::path_matches(&input.write_set, &lib_path)
            || input
                .lib_declarations
                .get(&lib_path)
                .is_some_and(|modules| modules.iter().any(|name| name == &module_name))
        {
            continue;
        }

        unreachable.push(format!(
            "task {} 新模块 {} 在 HEAD 不存在，且 {} 缺少 `pub mod {};`；\
             请由 planner 在 HEAD 预置 `pub mod {};`，或把 {} 加入同卡 writeSet",
            input.task_id, module_path, lib_path, module_name, module_name, lib_path
        ));
    }
    unreachable.sort();
    unreachable.dedup();
    (!unreachable.is_empty()).then(|| unreachable.join("; "))
}

fn crate_root_module(path: &str) -> Option<(String, String)> {
    let mut components = path.split('/');
    if components.next()? != "orch" || components.next()? != "crates" {
        return None;
    }
    let crate_name = components.next()?;
    if crate_name.is_empty() || components.next()? != "src" {
        return None;
    }
    let file_name = components.next()?;
    if components.next().is_some() {
        return None;
    }
    let module_name = file_name.strip_suffix(".rs")?;
    if module_name.is_empty() || matches!(module_name, "lib" | "main") {
        return None;
    }
    Some((
        format!("orch/crates/{crate_name}/src/lib.rs"),
        module_name.to_string(),
    ))
}

fn public_module_declarations(source: &str) -> Vec<String> {
    let uncommented = rust_source_without_comments(source);
    let mut modules = uncommented
        .lines()
        .filter_map(|line| {
            let code = line.trim();
            let after_pub = code.strip_prefix("pub")?;
            if !after_pub.starts_with(char::is_whitespace) {
                return None;
            }
            let after_mod = after_pub.trim_start().strip_prefix("mod")?;
            if !after_mod.starts_with(char::is_whitespace) {
                return None;
            }
            let declaration = after_mod.trim_start();
            let semicolon = declaration.find(';')?;
            let module_name = declaration[..semicolon].trim();
            let suffix = declaration[semicolon + 1..].trim();
            if module_name.is_empty()
                || !module_name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                || (!suffix.is_empty() && !suffix.starts_with("/*"))
            {
                return None;
            }
            Some(module_name.to_string())
        })
        .collect::<Vec<_>>();
    modules.sort();
    modules.dedup();
    modules
}

fn rust_source_without_comments(source: &str) -> String {
    let characters = source.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(source.len());
    let mut index = 0;
    let mut block_depth = 0usize;
    while index < characters.len() {
        let current = characters[index];
        let next = characters.get(index + 1).copied();
        if block_depth > 0 {
            if current == '/' && next == Some('*') {
                block_depth += 1;
                output.push_str("  ");
                index += 2;
            } else if current == '*' && next == Some('/') {
                block_depth -= 1;
                output.push_str("  ");
                index += 2;
            } else {
                output.push(if current == '\n' { '\n' } else { ' ' });
                index += 1;
            }
        } else if current == '/' && next == Some('/') {
            while index < characters.len() && characters[index] != '\n' {
                output.push(' ');
                index += 1;
            }
        } else if current == '/' && next == Some('*') {
            block_depth = 1;
            output.push_str("  ");
            index += 2;
        } else {
            output.push(current);
            index += 1;
        }
    }
    output
}

fn module_reachability_error_at_head(
    root: &Path,
    task_id: &str,
    write_set: &[String],
) -> Result<Option<String>> {
    let candidates = write_set
        .iter()
        .filter_map(|path| {
            crate_root_module(path)
                .map(|(lib_path, module_name)| (path.clone(), lib_path, module_name))
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut head_files = BTreeSet::new();
    let mut lib_declarations = BTreeMap::new();
    for (module_path, lib_path, _) in &candidates {
        if crate::gitx::show_bytes(root, "HEAD", module_path).is_ok() {
            head_files.insert(module_path.clone());
        }
        if lib_declarations.contains_key(lib_path) {
            continue;
        }
        let declarations = match crate::gitx::show_bytes(root, "HEAD", lib_path) {
            Ok(bytes) => {
                head_files.insert(lib_path.clone());
                let source = std::str::from_utf8(&bytes)
                    .with_context(|| format!("HEAD 中 {lib_path} 必须为 UTF-8 Rust source"))?;
                public_module_declarations(source)
            }
            Err(_) => Vec::new(),
        };
        lib_declarations.insert(lib_path.clone(), declarations);
    }

    Ok(module_reachability_error(&ModuleReachabilityInput {
        task_id: task_id.to_string(),
        write_set: write_set.to_vec(),
        head_files,
        lib_declarations,
    }))
}

/// B147 · 可派发判定的显式依赖分量：返回 `depends_on` 中尚未 Recorded 的
/// 前驱清单（保持卡面声明序，不去重——重复声明即重复阻塞，与卡面事实一致）。
/// 返回空 = 显式依赖全部满足；非空 = 任务绝不可派发（阻塞而非仅排序）。
pub fn task_dependency_blockers(depends_on: &[String], recorded: &[String]) -> Vec<String> {
    depends_on
        .iter()
        .filter(|dependency| !recorded.iter().any(|done| done == *dependency))
        .cloned()
        .collect()
}

/// Deterministic topological order of task IDs from a round-wide dependency
/// graph (B141).
///
/// `edges` maps a task ID to the task IDs it depends on. A task may only be
/// scheduled once every dependency has been scheduled; same-tier members
/// break ties by lexicographic task ID so the order is reproducible. A cycle
/// fails closed by naming its members, and an edge onto a task ID that is not
/// a key in `edges` is a ghost dependency that fails closed too.
pub fn card_dependency_order(edges: &BTreeMap<String, Vec<String>>) -> Result<Vec<String>, String> {
    for (task, deps) in edges {
        for dep in deps {
            if !edges.contains_key(dep) {
                return Err(format!(
                    "task {task} depends on {dep}, which is not part of this round"
                ));
            }
        }
    }

    let mut order = Vec::with_capacity(edges.len());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut remaining: Vec<String> = edges.keys().cloned().collect();
    let mut guard = 0usize;
    while !remaining.is_empty() {
        guard += 1;
        if guard > edges.len() {
            let mut cycle: Vec<String> = remaining.clone();
            cycle.sort();
            return Err(format!("card dependsOn 存在环: {}", cycle.join(", ")));
        }
        let before = remaining.len();
        let mut ready: Vec<String> = remaining
            .iter()
            .filter(|task| edges[*task].iter().all(|dep| placed.contains(dep)))
            .cloned()
            .collect();
        ready.sort();
        for task in ready {
            order.push(task.clone());
            placed.insert(task.clone());
            let position = remaining.iter().position(|value| value == &task).unwrap();
            remaining.remove(position);
        }
        if remaining.len() == before {
            let mut cycle: Vec<String> = remaining.clone();
            cycle.sort();
            return Err(format!("card dependsOn 存在环: {}", cycle.join(", ")));
        }
    }
    Ok(order)
}

/// Prove a card's declared capability requirements are backed by the assigned
/// agent's scheduling roles (B141).
///
/// `required` must be a subset of `agent_roles`; otherwise the missing
/// capability is named. An empty `required` list is always satisfied.
pub fn capability_supported(required: &[String], agent_roles: &[String]) -> Result<(), String> {
    let missing: Vec<&String> = required
        .iter()
        .filter(|capability| !agent_roles.contains(capability))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "card 声明 capabilities 但 agent 缺 roles: {}",
            missing
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// IR-structure files: editing one of these changes the shape of `RoundIr` /
/// `IrTask` / `CardMeta` or the validation-digest computation surface, so two
/// concurrently-scheduled cards that both write the same member must be
/// serialized by an explicit `dependsOn` edge (B154 · H20). Add new members
/// here when the IR-shape contract gains new defining files.
const IR_STRUCTURE_FILES: &[&str] = &[
    "orch/crates/orch-host/src/plan.rs",
    "orch/crates/orch-host/src/card.rs",
];

/// B154 · H20 plan-time guard for the same-IR-file conflict (r51/B147×B150).
///
/// Two cards whose `writeSet`s both touch the same IR-structure file must be
/// serialized by a declared `dependsOn` edge; otherwise the second merge is
/// doomed to textually conflict on the IR shape and the validation-digest
/// face, which is exactly what stranded r51. Ordinary shared files (anything
/// outside [`IR_STRUCTURE_FILES`]) are the scheduler/writeSet's concern, not
/// this guard's — they may be permitted to serialize independently.
///
/// `a`/`b` are `(task_id, &writeSet)` pairs; `dependency_declared` is true when
/// either card declares a `dependsOn` edge onto the other (the DAG then
/// serializes them). On conflict the error names the offending file and both
/// task IDs so `orch plan` can refuse with an actionable message; otherwise
/// `Ok(())`.
pub fn same_ir_file_conflict(
    a: (&str, &[String]),
    b: (&str, &[String]),
    dependency_declared: bool,
) -> Result<(), String> {
    if dependency_declared {
        return Ok(());
    }
    let (id_a, write_a) = a;
    let (id_b, write_b) = b;
    if let Some(file) = IR_STRUCTURE_FILES
        .iter()
        .copied()
        .find(|file| write_a.iter().any(|w| w == file) && write_b.iter().any(|w| w == file))
    {
        return Err(format!(
            "tasks {id_a} 与 {id_b} 同时写 IR 结构文件 {file} 且未声明 dependsOn；\
             在其中一张卡上声明 dependsOn: [另一张] 以串行化（B154）"
        ));
    }
    Ok(())
}

/// B154 · run_plan_locked 的两两配对驱动：对全轮任务按 (i<j) 配对调用
/// [`same_ir_file_conflict`]，依赖声明为「任一方向存在 dependsOn 边」
/// （`a.depends_on ∋ b` 或 `b.depends_on ∋ a`）。返回的每个错误字符串已自带
/// task 上下文，可直接并入 `crosscheck_errors`。顺序稳定（按 tasks 顺序）。
fn collect_same_ir_file_conflicts(
    tasks: &[TaskInput],
    edges: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (i, a) in tasks.iter().enumerate() {
        for b in tasks.iter().skip(i + 1) {
            let dependency_declared = edges.get(&a.id).is_some_and(|deps| deps.contains(&b.id))
                || edges.get(&b.id).is_some_and(|deps| deps.contains(&a.id));
            if let Err(error) = same_ir_file_conflict(
                (&a.id, &a.write_set),
                (&b.id, &b.write_set),
                dependency_declared,
            ) {
                errors.push(error);
            }
        }
    }
    errors
}

fn task_card_id(binding_key: &str) -> String {
    Path::new(binding_key)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(binding_key)
        .to_string()
}

/// Return sorted task IDs whose complete card bytes are new or changed.
///
/// Source-binding maps use canonical card paths while pure contract tests may
/// use task IDs directly, so keys are normalized to their final file stem.
pub fn tasks_requiring_reverify(
    old_cards: &BTreeMap<String, String>,
    new_cards: &BTreeMap<String, String>,
) -> Vec<String> {
    let old = old_cards
        .iter()
        .map(|(key, digest)| (task_card_id(key), digest))
        .collect::<BTreeMap<_, _>>();
    new_cards
        .iter()
        .filter_map(|(key, digest)| {
            let task_id = task_card_id(key);
            (old.get(&task_id).copied() != Some(digest)).then_some(task_id)
        })
        .collect()
}

fn validate_source_path(path: &str, label: &str) -> Result<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        bail!(
            "{label} 必须是仓库内 canonical relative path: {}",
            path.display()
        );
    }
    Ok(())
}

fn source_file_bytes(root: &Path, rel: &str, label: &str) -> Result<Vec<u8>> {
    validate_source_path(rel, label)?;
    let path = root.join(rel);
    let validate_chain = || -> Result<fs::Metadata> {
        let mut cursor = root.to_path_buf();
        let mut target = None;
        for component in Path::new(rel).components() {
            let std::path::Component::Normal(name) = component else {
                unreachable!("validate_source_path accepted only normal components")
            };
            cursor.push(name);
            let metadata = fs::symlink_metadata(&cursor)
                .with_context(|| format!("{label} stat 失败: {}", cursor.display()))?;
            if metadata.file_type().is_symlink() {
                bail!("{label} 不得经过 symlink: {}", cursor.display());
            }
            if cursor == path {
                if !metadata.file_type().is_file() {
                    bail!("{label} 必须是 regular file: {}", cursor.display());
                }
                target = Some(metadata);
            } else if !metadata.file_type().is_dir() {
                bail!("{label} parent 必须是目录: {}", cursor.display());
            }
        }
        target.context("source path 为空")
    };
    let before = validate_chain()?;
    let mut file =
        File::open(&path).with_context(|| format!("打开 {label} 失败: {}", path.display()))?;
    let handle_before = file.metadata()?;
    if !same_source_identity(&before, &handle_before) {
        bail!("{label} 在 stat/open 间被替换");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let handle_after = file.metadata()?;
    let after = validate_chain()?;
    if !same_source_identity(&handle_before, &handle_after)
        || !same_source_identity(&handle_after, &after)
        || handle_after.len() != bytes.len() as u64
    {
        bail!("{label} 读取期间 inode/path/bytes 漂移");
    }
    Ok(bytes)
}

#[cfg(unix)]
fn same_source_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_source_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

fn validate_source_bindings(round: &str, tasks: &[TaskInput]) -> Result<()> {
    for task in tasks {
        if !valid_source_sha256(&task.card_source_sha256) {
            bail!("task {} card source SHA-256 非 canonical", task.id);
        }
        if task.has_seeds && task.seed_source_sha256.is_empty() {
            bail!("task {} seed source fingerprints 缺失", task.id);
        }
        for (path, digest) in &task.seed_source_sha256 {
            validate_source_path(path, "seed source")?;
            if !valid_source_sha256(digest) {
                bail!("task {} seed source SHA-256 非 canonical: {path}", task.id);
            }
        }
        let expected_card = format!("coordination/rounds/{round}/tasks/{}.md", task.id);
        validate_source_path(&expected_card, "task card source")?;
    }
    Ok(())
}

fn validate_required_contract(task: &TaskInput) -> Result<()> {
    use std::collections::BTreeSet;

    if task.required_reviews.is_empty() {
        bail!("task {} requiredReviews 缺失", task.id);
    }
    let primary_count = task
        .required_reviews
        .iter()
        .filter(|review| review.role == "primary")
        .count();
    if primary_count != 1 {
        bail!("task {} requiredReviews 必须恰好一个 primary", task.id);
    }
    let mut roles = BTreeSet::new();
    let mut agents = BTreeSet::new();
    for review in &task.required_reviews {
        if !matches!(review.role.as_str(), "primary" | "secondary") {
            bail!(
                "task {} requiredReviews role 只能是 primary|secondary: {}",
                task.id,
                review.role
            );
        }
        let role_valid = safe_identity_component(&review.role);
        let agent_valid = safe_identity_component(&review.agent);
        if !role_valid || !agent_valid {
            bail!("task {} requiredReviews role/agent 不得为空", task.id);
        }
        if !roles.insert(review.role.as_str()) {
            bail!(
                "task {} requiredReviews role 重复: {}",
                task.id,
                review.role
            );
        }
        if !agents.insert(review.agent.as_str()) {
            bail!(
                "task {} requiredReviews agent 重复: {}",
                task.id,
                review.agent
            );
        }
        if review.agent == task.agent {
            bail!("task {} reviewer 不得等于实现 agent", task.id);
        }
    }
    validate_required_evidence(task)?;
    if let Some(attempt_id) = task.bootstrap_pre_signoff_attempt.as_deref() {
        if !safe_identity_component(attempt_id) {
            bail!(
                "task {} bootstrapPreSignoffAttempt 非安全 identity component: {attempt_id:?}",
                task.id
            );
        }
        let suffix = attempt_id
            .strip_prefix(&format!("{}-A", task.id))
            .unwrap_or_default();
        if suffix.len() < 4
            || !suffix.bytes().all(|byte| byte.is_ascii_digit())
            || suffix.bytes().all(|byte| byte == b'0')
        {
            bail!(
                "task {} bootstrapPreSignoffAttempt 必须是本 task canonical attempt: {attempt_id:?}",
                task.id
            );
        }
    }
    Ok(())
}

fn validate_required_evidence(task: &TaskInput) -> Result<()> {
    if task.required_evidence.is_empty() {
        bail!("task {} requiredEvidence 缺失", task.id);
    }
    let mut evidence = BTreeSet::new();
    for evidence_id in &task.required_evidence {
        let valid = safe_identity_component(evidence_id);
        if !valid {
            bail!("task {} requiredEvidence id 非法: {evidence_id:?}", task.id);
        }
        if !evidence.insert(evidence_id.as_str()) {
            bail!("task {} requiredEvidence 重复: {evidence_id}", task.id);
        }
    }
    Ok(())
}

fn validate_gate_refs(task: &TaskInput) -> Result<()> {
    use std::collections::BTreeSet;

    if task.gates_fast.is_empty() {
        bail!("task {} gates.fast 为空", task.id);
    }
    let mut seen = BTreeSet::new();
    for gate in &task.gates_fast {
        let safe = !gate.is_empty()
            && gate != "."
            && gate != ".."
            && gate
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !safe {
            bail!(
                "task {} gate ref 必须是单一安全 component: {gate:?}",
                task.id
            );
        }
        if !seen.insert(gate.as_str()) {
            bail!("task {} gate ref 重复: {gate}", task.id);
        }
    }
    Ok(())
}

fn validate_binding_lane_floor(task: &TaskInput, project_binding: &binding::Binding) -> Result<()> {
    fn refs<'a>(task_id: &str, lane: &str, values: &'a [String]) -> Result<BTreeSet<&'a str>> {
        let mut seen = BTreeSet::new();
        for value in values {
            let safe = !value.is_empty()
                && value != "."
                && value != ".."
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
            if !safe {
                bail!("task {task_id} binding gates.{lane} ref 非安全 component: {value:?}");
            }
            if !seen.insert(value.as_str()) {
                bail!("task {task_id} binding gates.{lane} ref 重复: {value}");
            }
        }
        Ok(seen)
    }

    refs(&task.id, "candidate", &project_binding.gates.candidate)?;
    let signed_floor = task
        .gates_fast
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let known = project_binding
        .commands
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();

    for (lane, declared, configured) in [
        (
            "merge",
            project_binding.gates.merge_declared(),
            project_binding.gates.merge.as_slice(),
        ),
        (
            "fast",
            project_binding.gates.fast_declared(),
            project_binding.gates.fast.as_slice(),
        ),
    ] {
        // Absence remains compatible with pre-lane binding fixtures, while an
        // explicitly empty list is a real signed weakening and fails below.
        if !declared {
            continue;
        }
        let configured = refs(&task.id, lane, configured)?;
        let missing = signed_floor
            .difference(&configured)
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "task {} binding gates.{lane} 弱于 signed card gates.fast，缺: {}",
                task.id,
                missing.join(", ")
            );
        }
        let unknown = configured.difference(&known).copied().collect::<Vec<_>>();
        if !unknown.is_empty() {
            bail!(
                "task {} binding gates.{lane} 含未知 command: {}",
                task.id,
                unknown.join(", ")
            );
        }
    }

    if project_binding.gates.candidate_declared() {
        if project_binding.gates.candidate.is_empty() {
            bail!("task {} binding gates.candidate 显式为空", task.id);
        }
        for (lane, declared, commands) in [
            (
                "merge",
                project_binding.gates.merge_declared(),
                project_binding.gates.merge.as_slice(),
            ),
            (
                "fast",
                project_binding.gates.fast_declared(),
                project_binding.gates.fast.as_slice(),
            ),
        ] {
            if !declared || commands.is_empty() {
                bail!(
                    "task {} binding gates.candidate 已声明但缺非空 gates.{lane} 强门/升级目标",
                    task.id
                );
            }
        }
    }
    Ok(())
}

pub fn compile_ir(
    round: &str,
    mode_yaml: &str,
    binding_yaml: &str,
    tasks: &[TaskInput],
) -> Result<RoundIr> {
    compile_ir_with_dispatched(round, mode_yaml, binding_yaml, tasks, &BTreeSet::new())
}

fn compile_ir_v3(round: &str, binding_yaml: &str, loaded: &LoadedTasks) -> Result<RoundIr> {
    binding::validate_v3_binding_shape(binding_yaml.as_bytes())?;
    let plan_binding: PlanBinding =
        serde_yaml::from_str(binding_yaml).context("解析 schema 3 PROJECT-BINDING 失败")?;
    let gate_binding = binding::parse_binding_bytes(binding_yaml.as_bytes())
        .map_err(anyhow::Error::msg)
        .context("解析 schema 3 gate lanes 失败")?;
    validate_source_bindings(round, &loaded.tasks)?;
    if loaded.tasks.is_empty() {
        bail!("schema 3 ROUND-IR tasks 不得为空");
    }

    let mut ir_tasks = Vec::with_capacity(loaded.tasks.len());
    for task in &loaded.tasks {
        if !task.agent.is_empty()
            || task.wall_minutes.is_some()
            || !task.required_reviews.is_empty()
            || task.bootstrap_pre_signoff_attempt.is_some()
        {
            bail!(
                "schema 3 task {} 含 legacy actor/review/budget 字段",
                task.id
            );
        }
        validate_gate_refs(task)?;
        validate_required_evidence(task)?;
        validate_binding_lane_floor(task, &gate_binding)?;
        if task.seed_protocol == "seeded-red" && !task.has_seeds {
            bail!("task {} 使用 seeded-red 但 seed 缺失", task.id);
        }
        for path in &task.write_set {
            let intersects_protected =
                card::path_matches(&plan_binding.scope.protected_paths, path)
                    || plan_binding
                        .scope
                        .protected_paths
                        .iter()
                        .any(|protected| card::path_matches(std::slice::from_ref(path), protected));
            if intersects_protected {
                bail!("task {} writeSet 命中 protected path: {path}", task.id);
            }
        }
        ir_tasks.push(IrTask {
            id: task.id.clone(),
            agent: String::new(),
            seed_protocol: task.seed_protocol.clone(),
            has_seeds: task.has_seeds,
            write_set: task.write_set.clone(),
            frozen_paths: task.frozen_paths.clone(),
            gates_fast: task.gates_fast.clone(),
            wall_minutes: 0,
            required_reviews: Vec::new(),
            review_fallbacks: Vec::new(),
            nongate_seats: Vec::new(),
            review_quorum: None,
            primary_pass_alone_satisfies: false,
            required_evidence: task.required_evidence.clone(),
            bootstrap_pre_signoff_attempt: None,
            depends_on: loaded
                .depends_on_by_task
                .get(&task.id)
                .cloned()
                .unwrap_or_default(),
            requirement: IrTaskRequirement::default(),
        });
    }

    let mut task_cards = BTreeMap::new();
    let mut seed_sources = BTreeMap::new();
    for task in &loaded.tasks {
        task_cards.insert(
            format!("coordination/rounds/{round}/tasks/{}.md", task.id),
            task.card_source_sha256.clone(),
        );
        for (path, digest) in &task.seed_source_sha256 {
            if seed_sources.insert(path.clone(), digest.clone()).is_some() {
                bail!("schema 3 seed source 重复: {path}");
            }
        }
    }

    Ok(RoundIr {
        schema_version: ACTORLESS_ROUND_IR_SCHEMA_VERSION,
        round: round.to_string(),
        revision: 1,
        mode_ref: String::new(),
        policy: IrPolicy::default(),
        budgets: IrBudget::default(),
        verification: IrVerification::default(),
        liveness: IrLiveness::default(),
        dispatch: IrDispatch::default(),
        scheduling: IrScheduling::default(),
        source_bindings: IrSourceBindings {
            mode_path: String::new(),
            mode_sha256: String::new(),
            binding_sha256: source_sha256(binding_yaml.as_bytes()),
            agent_registry_digest: String::new(),
            harness_registry_digest: String::new(),
            task_cards,
            seed_sources,
            v3_entry_points: loaded.entry_points_by_task.clone(),
        },
        tasks: ir_tasks,
        task_order: loaded.task_order.clone(),
        skipped: Vec::new(),
    })
}

fn compile_ir_with_dispatched(
    round: &str,
    mode_yaml: &str,
    binding_yaml: &str,
    tasks: &[TaskInput],
    dispatched_tasks: &BTreeSet<String>,
) -> Result<RoundIr> {
    let mode: ModeConfig = serde_yaml::from_str(mode_yaml).context("解析 ModeConfig 失败")?;
    let binding: PlanBinding =
        serde_yaml::from_str(binding_yaml).context("解析 PROJECT-BINDING 失败")?;
    let gate_binding = binding::parse_binding_bytes(binding_yaml.as_bytes())
        .map_err(anyhow::Error::msg)
        .context("解析 PROJECT-BINDING gate lanes 失败")?;
    validate_source_bindings(round, tasks)?;

    if mode.verification.mode != "root-manual-fixed-head" {
        bail!("verification.mode 必须为 root-manual-fixed-head");
    }
    let verifier = mode
        .agents
        .get("verifier")
        .context("agents.verifier 缺失")?;
    if verifier.adapter != "root-manual" {
        bail!("agents.verifier.adapter 必须为 root-manual");
    }
    if mode.liveness.monitor_seconds != 15
        || mode.liveness.working_stall_minutes != 10
        || mode.liveness.confirm_samples != 2
    {
        bail!("liveness 必须固定为 monitorSeconds=15/workingStallMinutes=10/confirmSamples=2");
    }
    if mode
        .liveness
        .termination_grace_seconds
        .is_some_and(|seconds| seconds == 0 || seconds > 600)
    {
        bail!("liveness.terminationGraceSeconds 必须位于 1..=600");
    }
    let registered_agents = mode
        .scheduling
        .capacities
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    hierarchy_structurally_valid(&mode.scheduling.allowed_agents, &registered_agents)
        .map_err(anyhow::Error::msg)?;
    if mode.scheduling.capacities.len() != mode.scheduling.allowed_agents.len() {
        bail!("scheduling.capacities 必须精确覆盖 allowedAgents");
    }
    for agent in &mode.scheduling.allowed_agents {
        let capacity = mode
            .scheduling
            .capacities
            .get(agent)
            .with_context(|| format!("scheduling.capacities 缺 agent: {agent}"))?;
        if capacity.agent == 0 || capacity.quota == 0 || capacity.roles.is_empty() {
            bail!("scheduling.capacities.{agent} 必须显式给出非零 agent/quota/roles");
        }
    }

    if let Some(merger) = mode.agents.get("merger") {
        if merger.same_as.as_deref() != Some("reviewer") {
            bail!("merger 必须以 sameAs: reviewer 配置");
        }
    }

    let verifier_present = mode.agents.contains_key("verifier");
    if binding
        .verification
        .independent_verifier
        .to_ascii_lowercase()
        .starts_with("required")
        && !verifier_present
    {
        bail!("independent verifier 为 required，但 agents.verifier 缺席");
    }

    for task in tasks {
        let scheduling_applies = !dispatched_tasks.contains(&task.id);
        if scheduling_applies {
            if !mode.scheduling.allowed_agents.contains(&task.agent)
                || !mode.scheduling.capacities.contains_key(&task.agent)
            {
                bail!("task {} agent {} 未获 scheduling 授权", task.id, task.agent);
            }
            if !mode.scheduling.capacities[&task.agent]
                .roles
                .iter()
                .any(|role| role == "implement")
            {
                bail!(
                    "task {} agent {} 缺 implement capability",
                    task.id,
                    task.agent
                );
            }
        }
        validate_required_contract(task)?;
        validate_gate_refs(task)?;
        validate_binding_lane_floor(task, &gate_binding)?;
        if scheduling_applies {
            for review in &task.required_reviews {
                if !mode.scheduling.allowed_agents.contains(&review.agent)
                    || !mode.scheduling.capacities.contains_key(&review.agent)
                {
                    bail!(
                        "task {} reviewer {} 未获 scheduling 授权",
                        task.id,
                        review.agent
                    );
                }
                let required_capability = format!("{}-review", review.role);
                if !mode.scheduling.capacities[&review.agent]
                    .roles
                    .iter()
                    .any(|role| role == &required_capability)
                {
                    bail!(
                        "task {} reviewer {} 缺 {} capability",
                        task.id,
                        review.agent,
                        required_capability
                    );
                }
            }
        }
        for path in &task.write_set {
            let intersects_protected = card::path_matches(&binding.scope.protected_paths, path)
                || binding
                    .scope
                    .protected_paths
                    .iter()
                    .any(|protected| card::path_matches(std::slice::from_ref(path), protected));
            if intersects_protected {
                bail!("task {} writeSet 命中 protected path: {path}", task.id);
            }
        }
        if !matches!(task.wall_minutes, Some(minutes) if minutes > 0) {
            bail!("task {} loop bound 缺失或为 0", task.id);
        }
        if task.seed_protocol == "seeded-red" && !task.has_seeds {
            bail!("task {} 使用 seeded-red 但 seed 缺失", task.id);
        }
    }
    if tasks
        .iter()
        .filter(|task| task.bootstrap_pre_signoff_attempt.is_some())
        .count()
        > 1
    {
        bail!("ROUND-IR 最多允许一个 bootstrapPreSignoffAttempt migration permit");
    }

    if has_tier_f(&mode) && !matches!(mode.budgets.round.max_model_wakes, Some(wakes) if wakes > 0)
    {
        bail!("Tier F agent 在场时 wake 上限必须存在且大于 0");
    }

    if mode.git.push_policy != "forbidden" || binding.git.push_policy != "forbidden" {
        bail!("push policy 在 v1 中必须为 forbidden");
    }

    let auto_merge_on_pass = mode.hitl.merge_gate == "auto";
    if auto_merge_on_pass && !verifier_present {
        bail!("mergeGate=auto 必须由 verifier PASS 守卫");
    }

    let skipped = vec![
        SkippedRule {
            rule: "3".into(),
            reason: "v1 relay 没有规则类任务，ruleAuditor 独立性不适用".into(),
        },
        SkippedRule {
            rule: "5".into(),
            reason: "v1 relay 串行执行，task writeSet 相交允许".into(),
        },
        SkippedRule {
            rule: "7".into(),
            reason: "adapter 能力快照在 M3 引入".into(),
        },
        SkippedRule {
            rule: "12".into(),
            reason: "Tier F 工作区可行性由 run_plan 的 IO 检查完成".into(),
        },
    ];

    let ir_tasks = tasks
        .iter()
        .map(|task| IrTask {
            id: task.id.clone(),
            agent: task.agent.clone(),
            seed_protocol: task.seed_protocol.clone(),
            has_seeds: task.has_seeds,
            write_set: task.write_set.clone(),
            frozen_paths: task.frozen_paths.clone(),
            gates_fast: task.gates_fast.clone(),
            wall_minutes: task
                .wall_minutes
                .expect("task wall_minutes was validated above"),
            required_reviews: task.required_reviews.clone(),
            review_fallbacks: Vec::new(),
            nongate_seats: Vec::new(),
            review_quorum: None,
            primary_pass_alone_satisfies: false,
            required_evidence: task.required_evidence.clone(),
            bootstrap_pre_signoff_attempt: task.bootstrap_pre_signoff_attempt.clone(),
            // B147：TaskInput 布局冻结不含 B141 元数据；卡面 dependsOn /
            // requirement 由 apply_card_metadata_overlay 在编译后统一覆写。
            depends_on: Vec::new(),
            requirement: IrTaskRequirement::default(),
        })
        .collect();

    let mut task_cards = BTreeMap::new();
    let mut seed_sources = BTreeMap::new();
    for task in tasks {
        task_cards.insert(
            format!("coordination/rounds/{round}/tasks/{}.md", task.id),
            task.card_source_sha256.clone(),
        );
        for (path, digest) in &task.seed_source_sha256 {
            if let Some(previous) = seed_sources.insert(path.clone(), digest.clone()) {
                if previous != *digest {
                    bail!("seed source {path} 在 task 间 SHA-256 冲突");
                }
            }
        }
    }

    Ok(RoundIr {
        schema_version: SCHEDULED_ROUND_IR_SCHEMA_VERSION,
        round: round.to_string(),
        revision: 1,
        mode_ref: String::new(),
        policy: IrPolicy {
            push_policy: mode.git.push_policy,
            merge_policy: mode.git.merge_policy,
            auto_merge_on_pass,
        },
        budgets: mode.budgets.round.into(),
        verification: IrVerification {
            mode: mode.verification.mode,
            adapter: verifier.adapter.clone(),
            model: verifier.model.clone(),
        },
        liveness: IrLiveness {
            monitor_seconds: mode.liveness.monitor_seconds,
            working_stall_minutes: mode.liveness.working_stall_minutes,
            confirm_samples: mode.liveness.confirm_samples,
            termination_grace_seconds: mode.liveness.termination_grace_seconds,
            stall_escalation_multiplier: mode.liveness.stall_escalation_multiplier,
            auto_terminate_stalled: mode.liveness.auto_terminate_stalled,
        },
        dispatch: IrDispatch {
            ack_timeout_seconds: mode.dispatch.ack_timeout_seconds,
        },
        scheduling: IrScheduling {
            allowed_agents: mode.scheduling.allowed_agents,
            capacities: mode.scheduling.capacities,
            agent_domains: BTreeMap::new(),
            quota_domains: mode.scheduling.quota_domains,
        },
        source_bindings: IrSourceBindings {
            mode_path: String::new(),
            mode_sha256: contract_digest_of_mode(mode_yaml)?,
            binding_sha256: source_sha256(binding_yaml.as_bytes()),
            agent_registry_digest: String::new(),
            harness_registry_digest: String::new(),
            task_cards,
            seed_sources,
            v3_entry_points: BTreeMap::new(),
        },
        tasks: ir_tasks,
        task_order: tasks.iter().map(|task| task.id.clone()).collect(),
        skipped,
    })
}

/// Decode schema 3 through its exact-key wire contract, or schema 1/2 through
/// the read-only legacy decoder. Unknown generations are rejected; successful
/// decoding alone grants no authority to write a round or dispatch a task.
pub fn parse_signed_round_ir(yaml: &str) -> std::result::Result<RoundIr, String> {
    let raw: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|error| format!("解析 signed ROUND-IR raw YAML 失败: {error}"))?;
    let mapping = raw
        .as_mapping()
        .ok_or_else(|| "signed ROUND-IR 顶层必须是 mapping".to_string())?;
    let schema = mapping
        .get(serde_yaml::Value::String("schemaVersion".to_string()))
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| "ROUND-IR schemaVersion 必须是 u32 integer".to_string())
        })
        .transpose()?
        .unwrap_or(LEGACY_ROUND_IR_SCHEMA_VERSION);
    if schema == ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        return serde_yaml::from_str::<RoundIrV3Wire>(yaml)
            .map_err(|error| format!("解析 schema 3 signed ROUND-IR 失败: {error}"))?
            .into_round_ir();
    }
    if !matches!(
        schema,
        LEGACY_ROUND_IR_SCHEMA_VERSION | SCHEDULED_ROUND_IR_SCHEMA_VERSION
    ) {
        return Err(format!(
            "ROUND-IR schemaVersion={schema} 未建模（仅支持 1/2/3）"
        ));
    }
    crate::legacy::decode_round_ir_v1_v2(yaml, schema)
}

/// Hash the historical canonical contract independently of dependency map-order features.
pub fn validation_digest(ir: &RoundIr) -> String {
    let mut contract = serde_json::to_value(ir).expect("RoundIr serialization is infallible");
    contract
        .as_object_mut()
        .expect("RoundIr serializes as an object")
        .remove("budgets");
    // Match historical sorted maps even when ACP enables preserve_order.
    contract.sort_all_objects();
    let normalized =
        serde_json::to_vec(&contract).expect("RoundIr contract serialization is infallible");
    let digest = Sha256::digest(normalized);
    hex::encode(digest)
}

fn parse_committed_policy_ledger(bytes: &[u8], label: &str) -> Result<Vec<EventRecord>> {
    let text = std::str::from_utf8(bytes).with_context(|| format!("{label} 非 UTF-8"))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<EventRecord>(line)
                .with_context(|| format!("{label} 第 {} 行非 canonical event", index + 1))
        })
        .collect()
}

fn yaml_policy_node<'a>(
    binding: &'a serde_yaml::Value,
    policy: &str,
) -> Result<&'a serde_yaml::Value> {
    let key = |value: &str| serde_yaml::Value::String(value.to_string());
    let root = binding
        .as_mapping()
        .context("committed binding root 必须是 mapping")?;
    let runtime_value = root
        .get(&key("runtimePolicies"))
        .context("committed binding 缺 runtimePolicies envelope")?;
    let runtime = runtime_value
        .as_mapping()
        .context("committed binding present runtimePolicies 非 mapping")?;
    if runtime.len() != 2
        || runtime
            .get(&key("schemaVersion"))
            .and_then(serde_yaml::Value::as_u64)
            != Some(1)
    {
        bail!("runtimePolicies envelope 必须是 exact schemaVersion 1 + policies");
    }
    runtime
        .get(&key("policies"))
        .and_then(serde_yaml::Value::as_mapping)
        .and_then(|policies| policies.get(&key(policy)))
        .with_context(|| format!("committed binding 缺 runtime policy {policy}"))
}

fn policy_node_string(node: &serde_yaml::Value, key: &str) -> Result<String> {
    node.as_mapping()
        .and_then(|mapping| mapping.get(&serde_yaml::Value::String(key.to_string())))
        .and_then(serde_yaml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .with_context(|| format!("runtime policy descriptor 缺非空 {key}"))
}

fn policy_node_u64(node: &serde_yaml::Value, key: &str) -> Result<u64> {
    node.as_mapping()
        .and_then(|mapping| mapping.get(&serde_yaml::Value::String(key.to_string())))
        .and_then(serde_yaml::Value::as_u64)
        .with_context(|| format!("runtime policy descriptor 缺 unsigned {key}"))
}

fn policy_node_carry_forward(
    node: &serde_yaml::Value,
) -> Result<Option<RuntimePolicyCarryForwardV1>> {
    let Some(value) = node
        .as_mapping()
        .and_then(|mapping| mapping.get(&serde_yaml::Value::String("carryForward".to_string())))
    else {
        return Ok(None);
    };
    Ok(Some(
        serde_yaml::from_value(value.clone()).context("runtime policy carryForward 非 exact")?,
    ))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

// Preserve EventRecord's established typed field order while normalizing only
// opaque JSON maps, which were sorted before preserve_order feature unification.
fn runtime_policy_event_bytes(event: &EventRecord) -> Result<Vec<u8>> {
    let mut event=event.clone();
    if let Some(payload)=&mut event.payload {payload.sort_all_objects();}
    event.extra.sort_keys();
    for value in event.extra.values_mut() {value.sort_all_objects();}
    Ok(serde_json::to_vec(&event)?)
}

fn policy_config_sha256(node: &serde_yaml::Value) -> Result<String> {
    let mut value =
        serde_json::to_value(node).context("runtime policy config 无法 canonicalize")?;
    let object = value
        .as_object_mut()
        .context("runtime policy descriptor 必须是 mapping")?;
    object.remove("initialState");
    object.remove("carryForward");
    value.sort_all_objects();
    Ok(sha256_bytes(&serde_json::to_vec(&value)?))
}

fn validate_policy_base_sha(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("policyBaseSha 必须为完整 40 位小写 hex commit");
    }
    Ok(())
}

/// One closed V1 command invocation whose immutable argv prefix comes from a
/// committed binding and whose optional selector is derived from signed card
/// or source-reader inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommandArgvV1 {
    /// Semantic command identity recorded by gate receipts.
    pub command_ref: String,
    /// Command whose argv prefix must exist in the committed binding.
    /// `sourceReaderClosure` deliberately uses the signed `seedTargets` Cargo
    /// test prefix; all other V1 commands bind to their own reference.
    pub binding_command_ref: String,
    /// Empty for a static binding command, or exactly
    /// `-p <safe-package> --test <safe-test>` for one derived Rust test.
    pub derived_argv: Vec<String>,
}

fn safe_resolved_command_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_resolved_command_input(command: &ResolvedCommandArgvV1) -> Result<()> {
    if !safe_resolved_command_component(&command.command_ref)
        || !safe_resolved_command_component(&command.binding_command_ref)
    {
        bail!("resolved command ref 非安全 component");
    }
    let expected_binding = if command.command_ref == "sourceReaderClosure" {
        "seedTargets"
    } else {
        command.command_ref.as_str()
    };
    if command.binding_command_ref != expected_binding {
        bail!(
            "resolved command {} binding base 必须为 {}，实际 {}",
            command.command_ref,
            expected_binding,
            command.binding_command_ref
        );
    }
    if command.derived_argv.is_empty() {
        if matches!(
            command.command_ref.as_str(),
            "seedTargets" | "sourceReaderClosure"
        ) {
            bail!(
                "resolved command {} 缺 signed-derived Rust test selector",
                command.command_ref
            );
        }
        return Ok(());
    }
    if !matches!(
        command.command_ref.as_str(),
        "seedTargets" | "sourceReaderClosure"
    ) || command.derived_argv.len() != 4
        || command.derived_argv[0] != "-p"
        || command.derived_argv[2] != "--test"
        || !safe_resolved_command_component(&command.derived_argv[1])
        || !safe_resolved_command_component(&command.derived_argv[3])
    {
        bail!(
            "resolved command {} derived argv 必须是 exact `-p <package> --test <test>`",
            command.command_ref
        );
    }
    Ok(())
}

/// Recompute the digest of ordered resolved argv from one immutable policy
/// base's committed binding.
///
/// Every argv prefix is reread from `policy_base_sha`; callers can supply only
/// the closed V1 Rust-test selector suffix, never a replacement executable or
/// base flag. The hash domain includes semantic command reference and complete
/// reconstructed argv in caller order. Unknown bases and empty argv are
/// rejected. Machine overlays and working-tree bytes are
/// intentionally excluded so B304 reuse identity describes the signed command
/// contract rather than a mutable local path. Repeated invocations remain in
/// the canonical vector: their position is part of the digest and receipt
/// sequence, so repeated semantic refs are never collapsed into a set.
pub fn resolved_command_argv_digest_at_policy_base(
    root: &Path,
    policy_base_sha: &str,
    ordered_commands: &[ResolvedCommandArgvV1],
) -> Result<String> {
    validate_policy_base_sha(policy_base_sha)?;
    if gitx::rev_parse(root, &format!("{policy_base_sha}^{{commit}}"))? != policy_base_sha
        || !gitx::is_ancestor(root, policy_base_sha, "refs/heads/main")?
    {
        bail!("policyBaseSha 必须是 current main 的 exact commit ancestor");
    }
    if ordered_commands.is_empty() {
        bail!("resolved command argv digest 拒绝空 command set");
    }

    let bytes = gitx::show_bytes(root, policy_base_sha, "coordination/PROJECT-BINDING.yaml")
        .context("读取 policy-base committed PROJECT-BINDING 失败")?;
    let committed = binding::parse_binding_bytes(&bytes)
        .map_err(anyhow::Error::msg)
        .context("解析 policy-base committed PROJECT-BINDING 失败")?;

    let mut resolved = Vec::with_capacity(ordered_commands.len());
    for command in ordered_commands {
        validate_resolved_command_input(command)?;
        let spec = committed
            .commands
            .get(&command.binding_command_ref)
            .with_context(|| {
                format!(
                    "policy-base binding 缺 command {}",
                    command.binding_command_ref
                )
            })?;
        if spec.argv.is_empty() {
            bail!(
                "policy-base binding command {} argv 为空",
                command.binding_command_ref
            );
        }
        if !command.derived_argv.is_empty() && spec.argv.iter().any(|argument| argument == "--") {
            bail!(
                "policy-base binding command {} 含 `--` terminator，拒绝把 derived selector 追加到其后",
                command.binding_command_ref
            );
        }
        let mut argv = spec.argv.clone();
        argv.extend(command.derived_argv.iter().cloned());
        resolved.push((command.command_ref.as_str(), argv));
    }

    let canonical = serde_json::to_vec(&resolved).context("canonicalize resolved command argv")?;
    let mut digest = Sha256::new();
    digest.update(b"orch-resolved-command-argv-v1\0");
    digest.update(canonical);
    Ok(hex::encode(digest.finalize()))
}

fn safe_policy_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn canonical_runtime_round(value: &str) -> bool {
    value.strip_prefix('r').is_some_and(|digits| {
        !digits.is_empty()
            && !digits.starts_with('0')
            && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn validate_review_pool_policy(policy: &ReviewPoolPolicyV1) -> Result<()> {
    if policy.schema_version != 1
        || policy.scope != "round"
        || policy.minimum_passes != 2
        || !policy.require_primary_pass
        || policy.initial_seat_count != 3
        || policy.minimum_gate_eligible != 2
        || policy.maximum_business_retries != 1
        || policy.nongate_substitutes_role != "secondary"
        || !matches!(policy.initial_state.as_str(), "dormant" | "active")
    {
        bail!("review-pool-v1 descriptor contract 漂移");
    }
    card::validate_task_id(&policy.owner_task)?;
    if policy.candidates.len() < 4 {
        bail!("review-pool-v1 至少需要四个 signed candidates");
    }
    let mut agents = BTreeSet::new();
    for candidate in &policy.candidates {
        if !safe_policy_id(&candidate.agent)
            || !agents.insert(candidate.agent.as_str())
            || !matches!(candidate.role.as_str(), "primary" | "secondary" | "nongate")
            || candidate.lineage != candidate.role
            || candidate
                .fallback_for
                .as_deref()
                .is_some_and(|agent| !safe_policy_id(agent))
        {
            bail!("review-pool-v1 candidate union 非 canonical");
        }
    }
    if policy.carry_forward.as_ref().is_some_and(|carry| {
        !canonical_runtime_round(&carry.source_round)
            || !safe_policy_id(&carry.source_event_id)
            || !valid_source_sha256(&carry.source_policy_sha256)
            || !valid_source_sha256(&carry.source_event_sha256)
    }) {
        bail!("review-pool-v1 carryForward identity/digest 非 canonical");
    }
    Ok(())
}

fn verify_policy_carry_forward(
    root: &Path,
    policy_base_sha: &str,
    policy: &str,
    current_config_sha256: &str,
    carry: &RuntimePolicyCarryForwardV1,
) -> Result<String> {
    let rel = format!("coordination/rounds/{}/events.jsonl", carry.source_round);
    let bytes = gitx::show_bytes(root, policy_base_sha, &rel)
        .with_context(|| format!("读取 carryForward source ledger 失败: {rel}"))?;
    let events = parse_committed_policy_ledger(&bytes, "carryForward source ledger")?;
    ledger::validate_runtime_event_history_v1(&events, &carry.source_round)?;
    let matching = events
        .iter()
        .filter(|event| event.event_id == carry.source_event_id)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        bail!("carryForward sourceEventId 不唯一");
    };
    let Some(ledger::RuntimeEventPayloadV1::RuntimePolicyActivated(activation)) =
        ledger::decode_runtime_event_v1(event)?
    else {
        bail!("carryForward source event 不是 RuntimePolicyActivated");
    };
    if event.round.as_deref() != Some(carry.source_round.as_str())
        || activation.policy != policy
        || activation.policy_sha256 != carry.source_policy_sha256
    {
        bail!("carryForward source activation tuple 漂移");
    }
    let canonical = runtime_policy_event_bytes(event)?;
    if sha256_bytes(&canonical) != carry.source_event_sha256 {
        bail!("carryForward sourceEventSha256 漂移");
    }
    let mut source_active = None::<String>;
    for candidate in &events {
        match ledger::decode_runtime_event_v1(candidate)? {
            Some(ledger::RuntimeEventPayloadV1::RuntimePolicyActivated(payload))
                if payload.policy == policy =>
            {
                source_active = Some(candidate.event_id.clone());
            }
            Some(ledger::RuntimeEventPayloadV1::RuntimePolicyDeactivated(payload))
                if payload.policy == policy
                    && source_active.as_deref() == Some(payload.activation_event_id.as_str()) =>
            {
                source_active = None;
            }
            _ => {}
        }
    }
    if source_active.as_deref() != Some(carry.source_event_id.as_str()) {
        bail!("carryForward source ledger 最终状态不是所指 activation");
    }
    let source_binding = gitx::show_bytes(
        root,
        &activation.activated_at_main_sha,
        "coordination/PROJECT-BINDING.yaml",
    )
    .context("读取 carryForward source binding 失败")?;
    let source_yaml: serde_yaml::Value =
        serde_yaml::from_slice(&source_binding).context("carryForward source binding 非 YAML")?;
    let source_node = yaml_policy_node(&source_yaml, policy)?;
    let mut source_raw = serde_json::to_value(source_node)?;
    source_raw.sort_all_objects();
    if sha256_bytes(&serde_json::to_vec(&source_raw)?) != carry.source_policy_sha256
        || policy_config_sha256(source_node)? != current_config_sha256
    {
        bail!("carryForward source/current stable policy config 漂移");
    }
    Ok(event.event_id.clone())
}

/// Resolve one runtime policy entirely from the committed binding, ROUND-IR,
/// and ledger blobs at `policy_base_sha`.  Mutable working-tree bytes and
/// policy events created after that commit are never consulted.
pub fn resolve_runtime_policy_at(
    root: &Path,
    round: &str,
    policy: &str,
    policy_base_sha: &str,
) -> Result<RuntimePolicyResolutionV1> {
    validate_policy_base_sha(policy_base_sha)?;
    if gitx::rev_parse(root, &format!("{policy_base_sha}^{{commit}}"))? != policy_base_sha
        || !gitx::is_ancestor(root, policy_base_sha, "refs/heads/main")?
    {
        bail!("policyBaseSha 必须是 current main 的 exact commit ancestor");
    }
    if !safe_policy_id(policy) {
        bail!("runtime policy id 非安全 component: {policy:?}");
    }
    let binding_rel = "coordination/PROJECT-BINDING.yaml";
    let binding_bytes = gitx::show_bytes(root, policy_base_sha, binding_rel)
        .context("读取 policy-base committed PROJECT-BINDING 失败")?;
    // Historical rounds can have both a committed binding and ROUND-IR while
    // predating runtimePolicies and the later source-binding digest.  Resolve
    // that compatibility discriminator first: callers may preserve the
    // signed legacy/quorum contract only for a genuinely absent policy
    // envelope, while every present envelope still takes the strict modern
    // IR/digest/ledger path below.
    let binding: serde_yaml::Value =
        serde_yaml::from_slice(&binding_bytes).context("policy-base binding YAML 非 canonical")?;
    let node = yaml_policy_node(&binding, policy)?;
    let binding_sha256 = sha256_bytes(&binding_bytes);
    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let ir_bytes = gitx::show_bytes(root, policy_base_sha, &ir_rel)
        .with_context(|| format!("读取 policy-base committed ROUND-IR 失败: {ir_rel}"))?;
    let ir_text = std::str::from_utf8(&ir_bytes).context("policy-base ROUND-IR 非 UTF-8")?;
    let ir = parse_signed_round_ir(ir_text).map_err(anyhow::Error::msg)?;
    if ir.round != round || ir.source_bindings.binding_sha256 != binding_sha256 {
        bail!("policy-base ROUND-IR round/bindingSha256 漂移");
    }
    // Once the envelope is present, every remaining committed input stays
    // mandatory; no legacy compatibility branch can bypass these checks.
    let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let ledger_bytes = gitx::show_bytes(root, policy_base_sha, &ledger_rel)
        .with_context(|| format!("读取 policy-base committed ledger 失败: {ledger_rel}"))?;
    let events = parse_committed_policy_ledger(&ledger_bytes, "policy-base ledger")?;
    ledger::validate_runtime_event_history_v1(&events, round)?;
    let digest = validation_digest(&ir);
    let mut validations = 0usize;
    for event in events.iter().filter(|event| event.kind == "TaskValidated") {
        let value = decode_runtime_task_validated(event, round)?;
        if value.ir_revision == ir.revision && value.validation_digest == digest {
            validations += 1;
        }
    }
    if validations != 1
        || matching_user_plan_signoff_positions(&events, round, ir.revision, &digest)?.len() != 1
    {
        bail!("policy-base ROUND-IR 缺唯一 committed validation/sign-off");
    }

    if policy_node_u64(node, "schemaVersion")? != 1 {
        bail!("runtime policy schemaVersion 未建模");
    }
    let owner_task = policy_node_string(node, "ownerTask")?;
    card::validate_task_id(&owner_task)?;
    let initial_state = policy_node_string(node, "initialState")?;
    if policy_node_string(node, "scope")? != "round" {
        bail!("runtime policy scope 必须为 round");
    }
    if !matches!(initial_state.as_str(), "dormant" | "active") {
        bail!("runtime policy initialState 必须为 dormant|active");
    }
    let mut policy_json =
        serde_json::to_value(node).context("runtime policy subtree 无法 canonicalize")?;
    policy_json.sort_all_objects();
    let policy_sha256 = sha256_bytes(&serde_json::to_vec(&policy_json)?);
    let config_sha256 = policy_config_sha256(node)?;
    let carry_forward = policy_node_carry_forward(node)?;
    if carry_forward.as_ref().is_some_and(|carry| {
        !canonical_runtime_round(&carry.source_round)
            || !safe_policy_id(&carry.source_event_id)
            || !valid_source_sha256(&carry.source_policy_sha256)
            || !valid_source_sha256(&carry.source_event_sha256)
    }) {
        bail!("runtime policy carryForward identity/digest 非 canonical");
    }
    let review_pool = if policy == "review-pool-v1" {
        let parsed: ReviewPoolPolicyV1 =
            serde_yaml::from_value(node.clone()).context("review-pool-v1 descriptor 非 exact")?;
        validate_review_pool_policy(&parsed)?;
        Some(parsed)
    } else {
        None
    };
    let (mut state, mut activation_event_id) = match initial_state.as_str() {
        "dormant" if carry_forward.is_none() => (RuntimePolicyStateV1::Dormant, None),
        "active" => {
            let carry = carry_forward.as_ref().context(
                "active runtime policy 必须显式 carryForward source round/event/policy digest",
            )?;
            (
                RuntimePolicyStateV1::Active,
                Some(verify_policy_carry_forward(
                    root,
                    policy_base_sha,
                    policy,
                    &config_sha256,
                    carry,
                )?),
            )
        }
        "dormant" => bail!("dormant runtime policy 不得携带 active carryForward"),
        _ => unreachable!(),
    };
    for event in &events {
        match ledger::decode_runtime_event_v1(event)? {
            Some(ledger::RuntimeEventPayloadV1::RuntimePolicyActivated(payload))
                if payload.policy == policy =>
            {
                if payload.owner_task != owner_task
                    || payload.binding_sha256 != binding_sha256
                    || payload.policy_sha256 != policy_sha256
                    || state == RuntimePolicyStateV1::Active
                {
                    bail!("policy-base activation tuple/state 漂移");
                }
                state = RuntimePolicyStateV1::Active;
                activation_event_id = Some(event.event_id.clone());
            }
            Some(ledger::RuntimeEventPayloadV1::RuntimePolicyDeactivated(payload))
                if payload.policy == policy =>
            {
                if state != RuntimePolicyStateV1::Active
                    || activation_event_id.as_deref() != Some(payload.activation_event_id.as_str())
                    || payload.binding_sha256 != binding_sha256
                    || payload.policy_sha256 != policy_sha256
                {
                    bail!("policy-base deactivation tuple/state 漂移");
                }
                state = RuntimePolicyStateV1::Dormant;
                activation_event_id = None;
            }
            _ => {}
        }
    }
    Ok(RuntimePolicyResolutionV1 {
        policy: policy.to_string(),
        owner_task,
        state,
        binding_sha256,
        policy_sha256,
        activation_event_id,
        policy_base_sha: policy_base_sha.to_string(),
        review_pool,
    })
}

/// Bind policy resolution to the unique first DispatchIssued of one exact
/// attempt, preventing callers from substituting current main or a later
/// policy transition as its policy base.
pub fn resolve_attempt_runtime_policy(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
    policy: &str,
) -> Result<RuntimePolicyResolutionV1> {
    card::validate_task_id(task_id)?;
    let dispatches = events
        .iter()
        .filter(|event| {
            event.kind == "DispatchIssued"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
        })
        .collect::<Vec<_>>();
    let [dispatch] = dispatches.as_slice() else {
        bail!("attempt policy resolution 要求唯一 DispatchIssued");
    };
    let attempt_no = dispatch
        .payload
        .as_ref()
        .and_then(|payload| payload.get("attemptNo"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .context("DispatchIssued 缺 positive attemptNo")?;
    if attempt_id != format!("{task_id}-A{attempt_no:04}") {
        bail!("DispatchIssued attemptId/attemptNo 非 canonical");
    }
    let base_sha = dispatch
        .payload
        .as_ref()
        .and_then(|payload| payload.get("baseSha"))
        .and_then(serde_json::Value::as_str)
        .context("DispatchIssued 缺 policy baseSha")?;
    resolve_runtime_policy_at(root, round, policy, base_sha)
}

/// Read the signed task workload from the same committed ROUND-IR used by a
/// policy-base resolver.  Review deadlines call the existing pure derivation
/// with this count instead of rereading a later working-tree card.
pub fn required_evidence_count_at_policy_base(
    root: &Path,
    round: &str,
    task_id: &str,
    policy_base_sha: &str,
) -> Result<usize> {
    validate_policy_base_sha(policy_base_sha)?;
    let rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let bytes = gitx::show_bytes(root, policy_base_sha, &rel)?;
    let text = std::str::from_utf8(&bytes).context("policy-base ROUND-IR 非 UTF-8")?;
    let ir = parse_signed_round_ir(text).map_err(anyhow::Error::msg)?;
    if ir.round != round {
        bail!("policy-base ROUND-IR round 漂移");
    }
    let task = ir
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("policy-base ROUND-IR 缺 task {task_id}"))?;
    Ok(task.required_evidence.len())
}

/// Reject a runtime-policy transition while the signed round still owns any
/// in-flight attempt, wake/review capacity, managed wake, merge barrier, or a
/// terminal `RoundClosed` fact.  CLI entrypoints and ledger append authority
/// both call this predicate so a lower-level constructor cannot bypass it.
pub(crate) fn ensure_runtime_policy_transition_idle(
    round: &str,
    events: &[EventRecord],
) -> Result<()> {
    if events.iter().any(|event| {
        event.kind == "RoundClosed"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
    }) {
        bail!("runtime policy transition 拒绝已关闭 round");
    }
    if crate::ledger::active_merge_barrier(events).is_some() {
        bail!("runtime policy transition 拒绝 active merge barrier");
    }
    let inflight = replan_invalidation_from_events(events, round)?;
    if !inflight.is_empty() {
        bail!(
            "runtime policy transition 拒绝在飞 attempt: {}",
            inflight.len()
        );
    }
    let load = crate::legacy::agent_inflight_load_from_events(events, round)
        .map_err(anyhow::Error::msg)?;
    if load.values().any(|items| !items.is_empty()) {
        bail!("runtime policy transition 拒绝在飞 wake/review capacity occupant");
    }
    for wake in events.iter().filter(|event| {
        event.kind == "WakeIssued"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("controlWakeId"))
                .and_then(serde_json::Value::as_str)
                .is_some()
    }) {
        let wake_id = wake
            .payload
            .as_ref()
            .and_then(|payload| payload.get("wakeId"))
            .and_then(serde_json::Value::as_str)
            .context("managed WakeIssued 缺 wakeId")?;
        let terminals = events
            .iter()
            .filter(|event| {
                event.kind == "ManagedWakeTerminated"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("wakeId"))
                        .and_then(serde_json::Value::as_str)
                        == Some(wake_id)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("managedScopeTerminated"))
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
            })
            .count();
        if terminals != 1 {
            bail!("runtime policy transition 拒绝未终结 managed wake {wake_id}");
        }
    }
    let mut actions = BTreeMap::<(String, String), String>::new();
    for event in events {
        let Some(prefix) = ["DispatchWake", "ResumeWake", "ReportCollect"]
            .into_iter()
            .find(|prefix| event.kind.starts_with(prefix))
        else {
            continue;
        };
        let action_id = event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("actionId"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("{} 缺 actionId", event.kind))?;
        actions.insert(
            (prefix.to_string(), action_id.to_string()),
            event.kind.clone(),
        );
    }
    if let Some(((prefix, action), phase)) = actions
        .iter()
        .find(|(_, phase)| !phase.ends_with("Released") && !phase.ends_with("Completed"))
    {
        bail!(
            "runtime policy transition 拒绝未释放 durable action: {prefix}:{action} phase={phase}"
        );
    }
    Ok(())
}

/// Refuse the retired live runtime-policy activation surface before any side effect.
pub fn activate_runtime_policy(
    _root: &Path,
    policy: &str,
) -> Result<RuntimePolicyTransitionOutcomeV1> {
    if policy.trim().is_empty() {
        bail!("runtime policy 必须是非空 identity");
    }
    bail!("runtime-policy production transition 已退役；历史 policy 仅允许 committed-tree 只读回放")
}

/// Refuse the retired live runtime-policy deactivation surface before any side effect.
pub fn deactivate_runtime_policy(
    _root: &Path,
    policy: &str,
    reason: &str,
) -> Result<RuntimePolicyTransitionOutcomeV1> {
    if policy.trim().is_empty() || reason.trim().is_empty() {
        bail!("runtime policy/reason 必须非空");
    }
    bail!("runtime-policy production transition 已退役；历史 policy 仅允许 committed-tree 只读回放")
}

#[derive(Clone, Copy)]
enum TaskLoadPurpose {
    AuthorizedPlan,
    ReadonlyReplay,
}

fn load_task_inputs(root: &Path, round: &str, purpose: TaskLoadPurpose) -> Result<LoadedTasks> {
    let contract_schema = if root
        .join(format!("coordination/rounds/{round}/events.jsonl"))
        .is_file()
    {
        crate::round::contract_schema_at_root(root, round)?
    } else {
        None
    };
    let schema_version = contract_schema.unwrap_or(SCHEDULED_ROUND_IR_SCHEMA_VERSION);
    let task_ids = task_ids(root, round)?;
    let mut tasks = Vec::with_capacity(task_ids.len());
    let mut crosscheck_errors = Vec::new();
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut capabilities_by_task: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut depends_on_by_task: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut complexity_by_task: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut review_fallbacks_by_task: BTreeMap<String, Vec<card::ReviewFallback>> = BTreeMap::new();
    let mut nongate_seats_by_task: BTreeMap<String, Vec<card::NongateSeat>> = BTreeMap::new();
    let mut review_quorum_by_task: BTreeMap<String, Option<card::ReviewQuorumPolicy>> =
        BTreeMap::new();
    let mut primary_pass_alone_satisfies_by_task: BTreeMap<String, bool> = BTreeMap::new();
    let mut entry_points_by_task: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for task_id in task_ids {
        let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
        let card_bytes = source_file_bytes(root, &card_rel, "task card source")?;
        let card_text = std::str::from_utf8(&card_bytes).context("task card 必须为 UTF-8")?;
        let loaded = card::parse(&card_rel, &task_id, card_text)?;
        if schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION {
            if loaded.meta.schema_version != Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
                bail!(
                    "schema 3 round {round} 的 task {} 必须显式 schemaVersion: 3",
                    loaded.meta.task_id
                );
            }
            if loaded.meta.round.as_deref() != Some(round) {
                bail!(
                    "schema 3 task {} round {:?} != current {round}",
                    loaded.meta.task_id,
                    loaded.meta.round
                );
            }
        } else if loaded.meta.schema_version == Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
            bail!("legacy round {round} 不得混入 schema 3 task card");
        }
        let seed_targets = loaded
            .meta
            .seeds
            .iter()
            .map(|seed| seed.target.clone())
            .collect::<Vec<_>>();
        if schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION
            && seed_targets.iter().collect::<BTreeSet<_>>().len() != seed_targets.len()
        {
            bail!("schema 3 task {} seed target 重复", loaded.meta.task_id);
        }
        let mut task_crosscheck_failed = false;
        if let Err(error) = seed_targets_covered_by_write_set(&seed_targets, &loaded.meta.write_set)
        {
            crosscheck_errors.push(format!("task {}: {error}", loaded.meta.task_id));
            task_crosscheck_failed = true;
        }
        if let Err(error) =
            write_set_disjoint_from_frozen(&loaded.meta.write_set, &loaded.meta.frozen_paths)
        {
            crosscheck_errors.push(format!("task {}: {error}", loaded.meta.task_id));
            task_crosscheck_failed = true;
        }
        // entryPoints became mandatory in r53. Archived cards and legacy
        // replay fixtures remain readable without a migration; every r53+
        // plan is held to the new authorization boundary.
        let entry_points_required =
            schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION || round_number_at_least(round, 53);
        if entry_points_required {
            if let Err(error) = card::entry_points_writable(
                &loaded.meta.entry_points,
                &loaded.meta.write_set,
                &loaded.meta.frozen_paths,
            ) {
                crosscheck_errors.push(format!("task {}: {error}", loaded.meta.task_id));
                task_crosscheck_failed = true;
            }
        }
        // H32 entered the plan contract in r57. Archived fixtures and signed
        // rounds retain their historical validation semantics; every r57+
        // plan evaluates new modules against its HEAD tree.
        if schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION || round_number_at_least(round, 57) {
            if let Some(error) = module_reachability_error_at_head(
                root,
                &loaded.meta.task_id,
                &loaded.meta.write_set,
            )? {
                crosscheck_errors.push(error);
                task_crosscheck_failed = true;
            }
        }
        // Planning is an authorization boundary too: an unsafe target must not
        // be allowed to produce either ROUND-IR or TaskValidated. Authorized
        // planning performs ref-free validation here and the shared exact-main
        // writeSet census after every card is loaded; readonly replay retains
        // the legacy seed-path contract without that census.
        if !task_crosscheck_failed {
            match purpose {
                TaskLoadPurpose::AuthorizedPlan
                    if schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION =>
                {
                    crate::oracle::validate_seed_paths_for_v3(root, &loaded)?
                }
                TaskLoadPurpose::AuthorizedPlan => {
                    crate::oracle::validate_seed_paths_for_authorized_plan(root, &loaded)?
                }
                TaskLoadPurpose::ReadonlyReplay => {
                    if schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION {
                        crate::oracle::validate_seed_paths_for_v3(root, &loaded)?
                    } else {
                        crate::oracle::validate_seed_paths(root, &loaded)?
                    }
                }
            }
        }
        // B141: complexity must parse into the closed enum, the whole-round
        // dependsOn graph must be acyclic and reference only round members,
        // and declared capabilities are stashed for the scheduling role
        // subset check once ModeConfig is available.
        if let Some(value) = loaded.meta.complexity.as_deref() {
            card::complexity_tier(value)
                .map_err(|error| anyhow::anyhow!("task {}: {error}", loaded.meta.task_id))?;
        }
        if edges
            .insert(loaded.meta.task_id.clone(), loaded.meta.depends_on.clone())
            .is_some()
        {
            bail!("task {} 重复声明", loaded.meta.task_id);
        }
        if capabilities_by_task
            .insert(
                loaded.meta.task_id.clone(),
                loaded.meta.capabilities.clone(),
            )
            .is_some()
        {
            bail!("task {} capabilities 重复登记", loaded.meta.task_id);
        }
        let wall_minutes = loaded.meta.budgets.wall_minutes;
        let card_source_sha256 = source_sha256(&card_bytes);
        let mut seed_source_sha256 = BTreeMap::new();
        for seed in &loaded.meta.seeds {
            let canonical_seed_prefix =
                format!("coordination/rounds/{round}/seeds/{}/", loaded.meta.task_id);
            if !seed.src.starts_with(&canonical_seed_prefix) {
                bail!(
                    "task {} seed src 必须位于 canonical seed 目录 {}",
                    loaded.meta.task_id,
                    canonical_seed_prefix
                );
            }
            let expected = seed
                .sha256
                .as_deref()
                .filter(|value| valid_source_sha256(value))
                .with_context(|| {
                    format!(
                        "task {} seed {} 缺 canonical declared sha256",
                        loaded.meta.task_id, seed.src
                    )
                })?;
            let bytes = source_file_bytes(root, &seed.src, "seed source")?;
            let actual = source_sha256(&bytes);
            if actual != expected {
                bail!(
                    "task {} seed {} SHA-256 不匹配: declared={} actual={}",
                    loaded.meta.task_id,
                    seed.src,
                    expected,
                    actual
                );
            }
            if let Some(previous) = seed_source_sha256.insert(seed.src.clone(), actual.clone()) {
                if schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION {
                    bail!(
                        "schema 3 task {} seed source {} 重复",
                        loaded.meta.task_id,
                        seed.src
                    );
                }
                if previous != actual {
                    bail!(
                        "task {} seed source {} 重复且 digest 冲突",
                        loaded.meta.task_id,
                        seed.src
                    );
                }
            }
        }
        let depends_on = loaded.meta.depends_on.clone();
        let complexity = loaded.meta.complexity.clone();
        let review_fallbacks = loaded.meta.review_fallbacks.clone();
        let nongate_seats = loaded.meta.nongate_seats.clone();
        let review_quorum = loaded.meta.review_quorum.clone();
        let primary_pass_alone_satisfies = loaded.meta.primary_pass_alone_satisfies;
        tasks.push(TaskInput {
            id: loaded.meta.task_id,
            agent: loaded.meta.agent.unwrap_or_default(),
            seed_protocol: loaded.meta.seed_protocol.unwrap_or_default(),
            has_seeds: !loaded.meta.seeds.is_empty(),
            write_set: loaded.meta.write_set,
            frozen_paths: loaded.meta.frozen_paths,
            gates_fast: loaded.meta.gates.fast,
            wall_minutes,
            required_reviews: loaded.meta.required_reviews,
            required_evidence: loaded.meta.required_evidence,
            bootstrap_pre_signoff_attempt: loaded.meta.bootstrap_pre_signoff_attempt,
            card_source_sha256,
            seed_source_sha256,
        });
        // B147：TaskInput 布局冻结（冻结测试直接构造），B141 卡面元数据经
        // LoadedTasks 侧车携带，编译后由 apply_card_metadata_overlay 覆写进 IR。
        depends_on_by_task.insert(tasks.last().expect("just pushed").id.clone(), depends_on);
        complexity_by_task.insert(tasks.last().expect("just pushed").id.clone(), complexity);
        review_fallbacks_by_task.insert(
            tasks.last().expect("just pushed").id.clone(),
            review_fallbacks,
        );
        nongate_seats_by_task.insert(tasks.last().expect("just pushed").id.clone(), nongate_seats);
        review_quorum_by_task.insert(tasks.last().expect("just pushed").id.clone(), review_quorum);
        primary_pass_alone_satisfies_by_task.insert(
            tasks.last().expect("just pushed").id.clone(),
            primary_pass_alone_satisfies,
        );
        entry_points_by_task.insert(
            tasks.last().expect("just pushed").id.clone(),
            loaded.meta.entry_points,
        );
    }
    // B154 · H20：全轮任务两两配对——同写一个 IR 结构文件且无显式 dependsOn
    // 边（任一方向）即 plan 整体拒绝。runs after every card is loaded so the
    // dependsOn graph is complete; matches the existing family (B136/B141).
    let pair_conflicts = collect_same_ir_file_conflicts(&tasks, &edges);
    for error in pair_conflicts {
        crosscheck_errors.push(error);
    }
    if !crosscheck_errors.is_empty() {
        bail!(
            "plan crosscheck 违规清单:\n- {}",
            crosscheck_errors.join("\n- ")
        );
    }
    let task_order = card_dependency_order(&edges).map_err(|error| anyhow::anyhow!(error))?;
    reorder_tasks(&mut tasks, &task_order)?;
    Ok(LoadedTasks {
        schema_version,
        tasks,
        task_order,
        capabilities_by_task,
        depends_on_by_task,
        complexity_by_task,
        review_fallbacks_by_task,
        nongate_seats_by_task,
        review_quorum_by_task,
        primary_pass_alone_satisfies_by_task,
        entry_points_by_task,
    })
}

/// Output of [`load_task_inputs`]: the task slice in deterministic topological
/// order plus the B141 card-declared capability requirements keyed by task ID.
/// B147 增补：dependsOn 边与 complexity 同样按 task ID 侧车携带（TaskInput
/// 布局冻结，元数据经 overlay 进 IR）。
struct LoadedTasks {
    schema_version: u32,
    tasks: Vec<TaskInput>,
    #[allow(dead_code)]
    task_order: Vec<String>,
    capabilities_by_task: BTreeMap<String, Vec<String>>,
    depends_on_by_task: BTreeMap<String, Vec<String>>,
    complexity_by_task: BTreeMap<String, Option<String>>,
    review_fallbacks_by_task: BTreeMap<String, Vec<card::ReviewFallback>>,
    nongate_seats_by_task: BTreeMap<String, Vec<card::NongateSeat>>,
    review_quorum_by_task: BTreeMap<String, Option<card::ReviewQuorumPolicy>>,
    primary_pass_alone_satisfies_by_task: BTreeMap<String, bool>,
    entry_points_by_task: BTreeMap<String, Vec<String>>,
}

/// B147：把卡面 B141 元数据（dependsOn 显式边 + requirement{complexity,
/// capabilities}）覆写进编译产物。digest 语义与 compile 时透传等价——
/// 所有产生 candidate/persisted IR 的路径都必须经过本覆写。
fn apply_card_metadata_overlay(ir: &mut RoundIr, loaded: &LoadedTasks) {
    for task in &mut ir.tasks {
        if let Some(depends_on) = loaded.depends_on_by_task.get(&task.id) {
            task.depends_on = depends_on.clone();
        }
        task.requirement = IrTaskRequirement {
            complexity: loaded.complexity_by_task.get(&task.id).cloned().flatten(),
            capabilities: loaded
                .capabilities_by_task
                .get(&task.id)
                .cloned()
                .unwrap_or_default(),
        };
        task.review_fallbacks = loaded
            .review_fallbacks_by_task
            .get(&task.id)
            .cloned()
            .unwrap_or_default();
        task.nongate_seats = loaded
            .nongate_seats_by_task
            .get(&task.id)
            .cloned()
            .unwrap_or_default();
        task.review_quorum = loaded
            .review_quorum_by_task
            .get(&task.id)
            .cloned()
            .flatten();
        task.primary_pass_alone_satisfies = loaded
            .primary_pass_alone_satisfies_by_task
            .get(&task.id)
            .copied()
            .unwrap_or(false);
    }
}

/// Validate the B141 card contract extensions (`complexity`, `dependsOn`,
/// `capabilities`) against the round's `ModeConfig` scheduling roles. The
/// `complexity` closed-enum parse and the whole-round `dependsOn` graph
/// (cycle + ghost-target fail-closed) are enforced by [`load_task_inputs`];
/// the agent role subset check needs the parsed mode so it runs after
/// [`compile_ir`] has validated the scheduling envelope.
fn validate_card_capability_subset(
    ir: &RoundIr,
    capabilities_by_task: &BTreeMap<String, Vec<String>>,
    dispatched_tasks: &BTreeSet<String>,
) -> Result<()> {
    for task in &ir.tasks {
        if dispatched_tasks.contains(&task.id) {
            continue;
        }
        let Some(required) = capabilities_by_task.get(&task.id) else {
            continue;
        };
        let roles = ir
            .scheduling
            .capacities
            .get(&task.agent)
            .map(|capacity| capacity.roles.as_slice())
            .unwrap_or(&[]);
        capability_supported(required, roles)
            .map_err(|error| anyhow::anyhow!("task {}: {error}", task.id))?;
    }
    Ok(())
}

/// Validate the signed review-extension surface after card metadata has been
/// overlaid onto the compiled IR. Historical tasks with no extension fields
/// remain in strict two-formal-seat mode and bypass this validator unchanged.
fn validate_review_contract_extensions(ir: &RoundIr) -> Result<()> {
    for task in &ir.tasks {
        let has_fallback = !task.review_fallbacks.is_empty();
        let has_extension = has_fallback
            || !task.nongate_seats.is_empty()
            || task.review_quorum.is_some()
            || task.primary_pass_alone_satisfies;
        if !has_extension {
            continue;
        }
        if ir.source_bindings.harness_registry_digest.is_empty() {
            bail!(
                "task {} review fallback/quorum 要求 sourceBindings.harnessRegistryDigest",
                task.id
            );
        }
        let policy = task.review_quorum.as_ref().with_context(|| {
            format!(
                "task {} fallbackAgent/nongateSeats 必须与显式 reviewQuorum 同时签入",
                task.id
            )
        })?;
        if task.nongate_seats.is_empty() {
            bail!("task {} reviewQuorum 要求非空 nongateSeats", task.id);
        }
        if policy.minimum_substantive < 2 {
            bail!(
                "task {} reviewQuorum.minimumSubstantive 必须至少为 2",
                task.id
            );
        }
        if !policy.nongate_may_substitute_failed_formal {
            bail!(
                "task {} r79 closed reviewQuorum 不接受关闭 nongate substitution",
                task.id
            );
        }
        if policy.minimum_nongate_pass_for_substitution == 0
            || policy.minimum_nongate_pass_for_substitution > task.nongate_seats.len()
        {
            bail!(
                "task {} minimumNongatePassForSubstitution 超出显式 nongateSeats 可达范围",
                task.id
            );
        }
        let maximum_voices = task.required_reviews.len() + task.nongate_seats.len();
        if policy.minimum_substantive > maximum_voices {
            bail!(
                "task {} minimumSubstantive={} 超过可达 unique voices={maximum_voices}",
                task.id,
                policy.minimum_substantive
            );
        }

        let mut occupied = BTreeSet::new();
        occupied.insert(task.agent.as_str());
        for review in &task.required_reviews {
            occupied.insert(review.agent.as_str());
        }
        let formal_by_role = task
            .required_reviews
            .iter()
            .map(|review| (review.role.as_str(), review))
            .collect::<BTreeMap<_, _>>();
        let mut fallback_roles = BTreeSet::new();
        for fallback in &task.review_fallbacks {
            if !safe_identity_component(&fallback.role)
                || !safe_identity_component(&fallback.fallback_agent)
                || !fallback_roles.insert(fallback.role.as_str())
            {
                bail!(
                    "task {} reviewFallbacks role/fallbackAgent 非法或重复: {}/{}",
                    task.id,
                    fallback.role,
                    fallback.fallback_agent
                );
            }
            let review = formal_by_role
                .get(fallback.role.as_str())
                .with_context(|| {
                    format!(
                        "task {} reviewFallbacks role 未匹配 requiredReviews: {}",
                        task.id, fallback.role
                    )
                })?;
            let fallback = fallback.fallback_agent.as_str();
            if !occupied.insert(fallback) {
                bail!(
                    "task {} fallbackAgent 重复或与 implementer/formal 重叠: {fallback}",
                    task.id
                );
            }
            let required_capability = format!("{}-review", review.role);
            let capacity = ir.scheduling.capacities.get(fallback).with_context(|| {
                format!(
                    "task {} fallbackAgent {fallback} 未获 scheduling 授权",
                    task.id
                )
            })?;
            if !ir
                .scheduling
                .allowed_agents
                .iter()
                .any(|agent| agent == fallback)
                || !capacity
                    .roles
                    .iter()
                    .any(|role| role == &required_capability)
            {
                bail!(
                    "task {} fallbackAgent {fallback} 缺 {required_capability} capability",
                    task.id
                );
            }
        }
        let mut nongate_agents = BTreeSet::new();
        for seat in &task.nongate_seats {
            if seat.agent == "executor-dsh" && seat.preset != "minimal" {
                bail!(
                    "task {} executor-dsh nongate preset 必须逐字为 minimal",
                    task.id
                );
            }
            if !safe_identity_component(&seat.agent)
                || !safe_identity_component(&seat.preset)
                || !nongate_agents.insert(seat.agent.as_str())
                || occupied.contains(seat.agent.as_str())
            {
                bail!(
                    "task {} nongateSeats agent/preset 非法、重复或与其它声音重叠: {}/{}",
                    task.id,
                    seat.agent,
                    seat.preset
                );
            }
            let capacity = ir.scheduling.capacities.get(&seat.agent).with_context(|| {
                format!(
                    "task {} nongate reviewer {} 未获 scheduling 授权",
                    task.id, seat.agent
                )
            })?;
            if !ir
                .scheduling
                .allowed_agents
                .iter()
                .any(|agent| agent == &seat.agent)
                || !capacity.roles.iter().any(|role| role == "nongate-review")
            {
                bail!(
                    "task {} nongate reviewer {} 缺 nongate-review capability",
                    task.id,
                    seat.agent
                );
            }
        }
    }
    Ok(())
}

fn reorder_tasks(tasks: &mut Vec<TaskInput>, order: &[String]) -> Result<()> {
    if tasks.len() != order.len() {
        bail!("task_order 与 tasks 长度不一致");
    }
    let mut by_id: BTreeMap<String, TaskInput> = tasks
        .drain(..)
        .map(|task| (task.id.clone(), task))
        .collect();
    let mut missing: Vec<String> = order
        .iter()
        .filter(|id| !by_id.contains_key(*id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        missing.sort();
        bail!("task_order 含未声明 task: {}", missing.join(", "));
    }
    for id in order {
        if let Some(task) = by_id.remove(id) {
            tasks.push(task);
        }
    }
    Ok(())
}

#[cfg(test)]
fn compare_candidate_with_persisted(
    round: &str,
    mode_yaml: &str,
    binding_yaml: &str,
    tasks: &[TaskInput],
    persisted_yaml: &str,
    loaded: &LoadedTasks,
    mode_binding: Option<(&str, &str)>,
) -> Result<ReadonlyIrValidation> {
    compare_candidate_with_persisted_for_dispatch(
        round,
        mode_yaml,
        binding_yaml,
        tasks,
        persisted_yaml,
        loaded,
        mode_binding,
        &BTreeSet::new(),
        None,
        None,
    )
}

fn agent_pin_amendments_for_validation(
    events: &[EventRecord],
    round: &str,
    revision: u32,
    persisted_digest: &str,
) -> Result<Vec<crate::registry::AgentPinAmendment>> {
    let signoff_position =
        matching_user_plan_signoff_position(events, round, revision, persisted_digest)?;
    let mut amendments = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == crate::registry::AGENT_PIN_AMENDED_EVENT_KIND
            && event.round.as_deref() == Some(round)
    }) {
        let amendment = crate::registry::decode_agent_pin_amendment_event(event, round)?;
        if amendment.ir_revision() != revision {
            continue;
        }
        let Some(signoff_position) = signoff_position else {
            bail!("AgentPinAmended 存在于尚未 PlanSignedOff 的 IR revision={revision}");
        };
        if position <= signoff_position {
            bail!("AgentPinAmended 必须晚于其绑定的 PlanSignedOff");
        }
        amendments.push(amendment);
    }
    Ok(amendments)
}

fn compare_candidate_with_persisted_for_dispatch(
    round: &str,
    mode_yaml: &str,
    binding_yaml: &str,
    tasks: &[TaskInput],
    persisted_yaml: &str,
    loaded: &LoadedTasks,
    mode_binding: Option<(&str, &str)>,
    dispatched_tasks: &BTreeSet<String>,
    registry_root: Option<&Path>,
    registry_events: Option<&[EventRecord]>,
) -> Result<ReadonlyIrValidation> {
    let persisted = parse_signed_round_ir(persisted_yaml).map_err(anyhow::Error::msg)?;
    let persisted_digest = validation_digest(&persisted);
    let mut candidate =
        compile_ir_with_dispatched(round, mode_yaml, binding_yaml, tasks, dispatched_tasks)?;
    let mut registry_digest_matches = true;
    let mut harness_registry_digest_matches = true;
    if let Some(root) = registry_root {
        // Registry projection is deliberately non-retroactive. A persisted IR
        // without the digest predates this binding and must remain replayable;
        // the next explicit `orch plan` upgrades it through the production
        // binder below. Once present, readonly validation keeps enforcing the
        // exact registry bytes and derived scheduling domains.
        if !persisted.source_bindings.agent_registry_digest.is_empty() {
            bind_agent_registry_projection(root, &mut candidate)?;
            let amendments = match registry_events {
                Some(events) => agent_pin_amendments_for_validation(
                    events,
                    round,
                    persisted.revision,
                    &persisted_digest,
                )?,
                None => Vec::new(),
            };
            let expected = expected_registry_digest_with_amendments(
                &persisted.source_bindings.agent_registry_digest,
                &amendments,
            )?;
            registry_digest_matches = candidate.source_bindings.agent_registry_digest == expected;
            // The durable delta chain proves that current registry bytes are a
            // permitted successor of the signed genesis.  Normalize only the
            // ephemeral contract view back to that genesis so the user's
            // original (revision, validationDigest) sign-off stays bound; all
            // other candidate fields continue to compare byte-for-byte.
            candidate.source_bindings.agent_registry_digest =
                persisted.source_bindings.agent_registry_digest.clone();
        }
        if !persisted.source_bindings.harness_registry_digest.is_empty() {
            bind_harness_registry_projection(root, &mut candidate)?;
            harness_registry_digest_matches = candidate.source_bindings.harness_registry_digest
                == persisted.source_bindings.harness_registry_digest;
            candidate.source_bindings.harness_registry_digest =
                persisted.source_bindings.harness_registry_digest.clone();
        }
    }
    if let Some((mode_ref, mode_path)) = mode_binding {
        bind_mode_source(&mut candidate, mode_ref, mode_path);
    }
    validate_card_capability_subset(&candidate, &loaded.capabilities_by_task, dispatched_tasks)?;
    apply_card_metadata_overlay(&mut candidate, loaded);
    validate_review_contract_extensions(&candidate)?;
    candidate.schema_version = persisted.schema_version;
    candidate.revision = persisted.revision;
    let digest_matches = registry_digest_matches
        && harness_registry_digest_matches
        && validation_digest(&candidate) == persisted_digest;
    // `verify.rs` is frozen in B156 and its expected-main byte-integrity
    // check consumes this readonly candidate field as a raw blob digest.
    // Persisted ROUND-IR keeps the canonical contract-projection hash; expose
    // the raw source hash only on the ephemeral validation view so the
    // orthogonal "current bytes == expected main bytes" fence stays intact.
    candidate.source_bindings.mode_sha256 = source_sha256(mode_yaml.as_bytes());
    Ok(ReadonlyIrValidation {
        persisted_revision: persisted.revision,
        persisted_digest,
        digest_matches,
        candidate,
    })
}

/// 从当前 mode/binding/cards 只读重编译 candidate IR，并以 persisted revision 归一化后
/// 比较语义 digest。不会调用 run_plan，也不会写 ROUND-IR、账本或工作区探针。
///
/// Readonly replay retains the historical seed-path validation contract but
/// deliberately does not rerun authorized-plan landed write-set admission
/// against a later, movable `main`.
pub fn validate_round_ir_readonly(root: &Path, round: &str) -> Result<ReadonlyIrValidation> {
    if crate::round::contract_schema_at_root(root, round)?
        == Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        return validate_round_ir_readonly_v3(root, round);
    }
    let selected = selected_mode_path(root, round)?;
    let mode_path = selected.path;
    let mode_rel = mode_path.strip_prefix(root)?.to_string_lossy().to_string();
    let mode_bytes = source_file_bytes(root, &mode_rel, "ModeConfig")?;
    let mode_yaml = std::str::from_utf8(&mode_bytes).context("ModeConfig 必须为 UTF-8")?;
    let binding_bytes =
        source_file_bytes(root, "coordination/PROJECT-BINDING.yaml", "PROJECT-BINDING")?;
    let binding_yaml =
        std::str::from_utf8(&binding_bytes).context("PROJECT-BINDING 必须为 UTF-8")?;
    let loaded = load_task_inputs(root, round, TaskLoadPurpose::ReadonlyReplay)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = match read_ledger(&ledger_path) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::NotFound => orch_core::LedgerRead {
            events: Vec::new(),
            bad_lines: Vec::new(),
        },
        Err(error) => return Err(error).context("读取 readonly plan ledger 失败"),
    };
    if !ledger_read.bad_lines.is_empty() {
        bail!("readonly plan 拒绝坏账本");
    }
    let dispatched_tasks = dispatched_tasks_from_events(&ledger_read.events, round)?;
    let ir_path = root.join(format!("coordination/rounds/{round}/ROUND-IR.yaml"));
    let ir_rel = ir_path.strip_prefix(root)?.to_string_lossy().to_string();
    let persisted_bytes = source_file_bytes(root, &ir_rel, "ROUND-IR")?;
    let persisted_yaml = std::str::from_utf8(&persisted_bytes).context("ROUND-IR 必须为 UTF-8")?;
    let mode_binding = selected
        .explicit
        .then_some((selected.mode_ref.as_str(), mode_rel.as_str()));
    compare_candidate_with_persisted_for_dispatch(
        round,
        &mode_yaml,
        &binding_yaml,
        &loaded.tasks,
        &persisted_yaml,
        &loaded,
        mode_binding,
        &dispatched_tasks,
        Some(root),
        Some(&ledger_read.events),
    )
}

fn validate_round_ir_readonly_v3(root: &Path, round: &str) -> Result<ReadonlyIrValidation> {
    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let persisted_bytes = source_file_bytes(root, &ir_rel, "schema 3 ROUND-IR")?;
    let persisted_yaml =
        std::str::from_utf8(&persisted_bytes).context("schema 3 ROUND-IR 必须为 UTF-8")?;
    let persisted = parse_signed_round_ir(persisted_yaml).map_err(anyhow::Error::msg)?;
    if persisted.schema_version != ACTORLESS_ROUND_IR_SCHEMA_VERSION || persisted.round != round {
        bail!("schema 3 readonly ROUND-IR generation/round 漂移");
    }
    let binding_bytes = source_file_bytes(
        root,
        "coordination/PROJECT-BINDING.yaml",
        "schema 3 binding",
    )?;
    binding::validate_v3_binding_shape(&binding_bytes)?;
    let binding_yaml =
        std::str::from_utf8(&binding_bytes).context("schema 3 binding 必须为 UTF-8")?;
    let loaded = load_task_inputs(root, round, TaskLoadPurpose::ReadonlyReplay)?;
    if loaded.schema_version != ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        bail!("schema 3 readonly task loader generation 漂移");
    }
    let mut candidate = compile_ir_v3(round, binding_yaml, &loaded)?;
    candidate.revision = persisted.revision;
    let persisted_digest = validation_digest(&persisted);
    let digest_matches = validation_digest(&candidate) == persisted_digest;
    Ok(ReadonlyIrValidation {
        persisted_revision: persisted.revision,
        persisted_digest,
        digest_matches,
        candidate,
    })
}

pub fn load_round_ir(root: &Path, round: &str) -> Result<RoundIr> {
    let path = root.join(format!("coordination/rounds/{round}/ROUND-IR.yaml"));
    let text = fs::read_to_string(&path)
        .with_context(|| format!("读取 ROUND-IR 失败: {}", path.display()))?;
    let ir = parse_signed_round_ir(&text)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("解析 ROUND-IR 失败: {}", path.display()))?;
    if ir.round != round {
        bail!("ROUND-IR round={} 与当前轮 {round} 不符", ir.round);
    }
    Ok(ir)
}

/// Load one task card from a single stable byte snapshot and prove those exact
/// bytes are the card source fingerprint carried by the validated IR.
pub fn load_bound_task_card(
    root: &Path,
    round: &str,
    task_id: &str,
    validation: &ReadonlyIrValidation,
) -> Result<card::Card> {
    card::validate_task_id(task_id)?;
    if validation.candidate.round != round {
        bail!("validated IR round 与 card 请求不一致");
    }
    let rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let expected = validation
        .candidate
        .source_bindings
        .task_cards
        .get(&rel)
        .with_context(|| format!("validated IR 未绑定 task card: {rel}"))?;
    let bytes = source_file_bytes(root, &rel, "validated task card")?;
    let actual = source_sha256(&bytes);
    if &actual != expected {
        bail!("task card bytes 与 validated IR source binding 不一致: {rel}");
    }
    let text = std::str::from_utf8(&bytes).context("validated task card 必须为 UTF-8")?;
    card::parse(&rel, task_id, text)
}

/// Fail-closed contract guard shared by every production entry point.  The
/// persisted IR must still be the readonly recompilation of mode/binding/card,
/// the ledger must contain its canonical validation event, and a user must
/// have signed that exact revision/digest tuple.
pub fn require_validated_round_ir(
    root: &Path,
    round: &str,
    events: &[EventRecord],
) -> Result<ReadonlyIrValidation> {
    let validation = validate_round_ir_readonly(root, round)?;
    if !validation.digest_matches {
        bail!("ROUND-IR 与 mode/binding/cards 漂移；先重新 orch plan");
    }

    let high = validation_high_water(events, round)?;
    let validated = high.as_ref().is_some_and(|(_, payload, production)| {
        *production
            && payload.ir_revision == validation.persisted_revision
            && payload.validation_digest == validation.persisted_digest
    });
    if !validated {
        bail!(
            "当前 ROUND-IR revision={} digest={} 不是 ledger 最高 canonical production TaskValidated",
            validation.persisted_revision,
            validation.persisted_digest
        );
    }
    Ok(validation)
}

pub fn require_active_round_ir(
    root: &Path,
    round: &str,
    events: &[EventRecord],
) -> Result<ReadonlyIrValidation> {
    let persisted = load_round_ir(root, round)?;
    if persisted.schema_version == ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        if persisted.source_bindings.binding_sha256.is_empty()
            || persisted.tasks.is_empty()
            || !persisted.mode_ref.is_empty()
            || !persisted.policy.push_policy.is_empty()
            || !persisted.policy.merge_policy.is_empty()
            || persisted.policy.auto_merge_on_pass
            || persisted.budgets.max_usd.is_some()
            || persisted.budgets.wall_minutes.is_some()
            || persisted.budgets.max_model_wakes.is_some()
            || persisted.verification != IrVerification::default()
            || persisted.liveness != IrLiveness::default()
            || persisted.dispatch != IrDispatch::default()
            || persisted.scheduling != IrScheduling::default()
            || !persisted.source_bindings.mode_path.is_empty()
            || !persisted.source_bindings.mode_sha256.is_empty()
            || !persisted.source_bindings.agent_registry_digest.is_empty()
            || !persisted.source_bindings.harness_registry_digest.is_empty()
            || persisted.source_bindings.v3_entry_points.len() != persisted.tasks.len()
            || persisted.tasks.iter().any(|task| {
                persisted
                    .source_bindings
                    .v3_entry_points
                    .get(&task.id)
                    .is_none_or(Vec::is_empty)
            })
            || !persisted.skipped.is_empty()
            || persisted.tasks.iter().any(|task| {
                !task.agent.is_empty()
                    || task.wall_minutes != 0
                    || !task.required_reviews.is_empty()
                    || !task.review_fallbacks.is_empty()
                    || !task.nongate_seats.is_empty()
                    || task.review_quorum.is_some()
                    || task.primary_pass_alone_satisfies
                    || task.bootstrap_pre_signoff_attempt.is_some()
                    || task.requirement != IrTaskRequirement::default()
            })
        {
            bail!("active schema 3 ROUND-IR 含 legacy policy/scheduling/actor/review 语义");
        }
    } else if persisted.policy.push_policy.is_empty()
        || persisted.policy.merge_policy.is_empty()
        || persisted.scheduling.allowed_agents.is_empty()
        || persisted.source_bindings.mode_sha256.is_empty()
        || persisted.source_bindings.binding_sha256.is_empty()
        || (!persisted.scheduling.agent_domains.is_empty()
            && persisted.source_bindings.agent_registry_digest.is_empty())
    {
        bail!("active ROUND-IR 缺必需 policy/scheduling/sourceBindings 语义段");
    }
    let validation = require_validated_round_ir(root, round, events)?;
    if !matching_user_plan_signoff(
        events,
        round,
        validation.persisted_revision,
        &validation.persisted_digest,
    )? {
        bail!(
            "当前 ROUND-IR revision={} digest={} 尚未绑定 PlanSignedOff",
            validation.persisted_revision,
            validation.persisted_digest
        );
    }
    Ok(validation)
}

pub fn run_plan(root: &Path) -> Result<PlanOutcome> {
    let round = crate::current_round(root)?;
    if crate::round::contract_schema_at_root(root, &round)?
        != Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        bail!("schema 1/2 plan writer 已退役；历史轮仅支持只读 replay");
    }
    crate::close::with_protocol_transition(root, "orch plan", || {
        let fresh_round = crate::current_round(root)?;
        if fresh_round != round
            || crate::round::contract_schema_at_root(root, &fresh_round)?
            != Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION)
        {
            bail!("schema 3 plan 写锁内 round generation 漂移");
        }
        run_plan_v3_locked(root, &fresh_round)
    })
}

/// Build the bytes and event that a historical schema 1/2 planner would have
/// produced. This function is strictly read-only; fixture code outside the
/// production crate may materialize the returned artifact.
pub fn build_legacy_plan_artifact_readonly(
    root: &Path,
    round: &str,
) -> Result<LegacyPlanArtifact> {
    let selected = selected_mode_path(root, round)?;
    let mode_path = selected.path;
    let mode_rel = mode_path.strip_prefix(root)?.to_string_lossy().to_string();
    let mode_bytes = source_file_bytes(root, &mode_rel, "ModeConfig")?;
    let mode_yaml = std::str::from_utf8(&mode_bytes).context("ModeConfig 必须为 UTF-8")?;
    let binding_bytes =
        source_file_bytes(root, "coordination/PROJECT-BINDING.yaml", "PROJECT-BINDING")?;
    let binding_yaml =
        std::str::from_utf8(&binding_bytes).context("PROJECT-BINDING 必须为 UTF-8")?;
    let admission_binding = binding::parse_binding_bytes(&binding_bytes)
        .map_err(anyhow::Error::msg)
        .context("解析 PROJECT-BINDING admission floor 失败")?;
    binding::validate_rust_gate_floors_at_root(root, &admission_binding).map_err(|errors| {
        anyhow::anyhow!("Rust 门参数 floor 校验失败:\n- {}", errors.join("\n- "))
    })?;

    let loaded = load_task_inputs(root, &round, TaskLoadPurpose::AuthorizedPlan)?;
    let tasks = &loaded.tasks;

    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = match read_ledger(&ledger_path) {
        Ok(read) => Some(read),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("读取 plan revision high-water 失败"),
    };
    if ledger_read
        .as_ref()
        .is_some_and(|read| !read.bad_lines.is_empty())
    {
        bail!("plan 拒绝坏账本");
    }
    let ledger_events = ledger_read
        .as_ref()
        .map(|read| read.events.as_slice())
        .unwrap_or_default();
    let dispatched_tasks = dispatched_tasks_from_events(ledger_events, &round)?;

    let mut ir =
        compile_ir_with_dispatched(&round, &mode_yaml, &binding_yaml, tasks, &dispatched_tasks)?;
    // Registry qualification and quota-domain projection are part of the
    // signed contract. This happens before any IR bytes or validation events
    // are published, so an invalid registry/config combination fails closed.
    bind_agent_registry_projection(root, &mut ir)?;
    bind_harness_registry_projection(root, &mut ir)?;
    // Planning upgrades the legacy one-mode layout to an explicit binding.
    // Readonly validation preserves old unbound IRs until that deliberate
    // replan, so archived/signed rounds remain replayable.
    bind_mode_source(&mut ir, &selected.mode_ref, &mode_rel);
    validate_card_capability_subset(&ir, &loaded.capabilities_by_task, &dispatched_tasks)?;
    apply_card_metadata_overlay(&mut ir, &loaded);
    validate_review_contract_extensions(&ir)?;
    let ir_path = root.join(format!("coordination/rounds/{round}/ROUND-IR.yaml"));
    let ir_rel = ir_path.strip_prefix(root)?.to_string_lossy().to_string();
    let existing = match fs::symlink_metadata(&ir_path) {
        Ok(_) => {
            let bytes = source_file_bytes(root, &ir_rel, "ROUND-IR")?;
            let text = std::str::from_utf8(&bytes).context("ROUND-IR 必须为 UTF-8")?;
            Some(
                parse_signed_round_ir(text)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| format!("解析既有 ROUND-IR 失败: {}", ir_path.display()))?,
            )
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取既有 ROUND-IR 失败: {}", ir_path.display()))
        }
    };

    // The first persisted IR is bootstrap validation, not a replan.  Existing
    // seed/sign-off ordering remains compatible there; only a later persisted
    // revision can create must-reverify work.
    let card_reverify_tasks = existing
        .as_ref()
        .map(|value| {
            tasks_requiring_reverify(
                &value.source_bindings.task_cards,
                &ir.source_bindings.task_cards,
            )
        })
        .unwrap_or_default();
    let contract_changed = reconcile_revision(&mut ir, existing.as_ref())?;
    // The first persisted IR is bootstrap validation, even when a signed
    // one-shot bootstrap attempt predates it. Only replacing an existing
    // contract is a replan capable of invalidating prior authorization.
    ensure_replan_safe(
        round_number_at_least(&round, 53) && existing.is_some() && contract_changed,
        ledger_events,
        &round,
    )?;
    let ledger_high = validation_high_water(
        ledger_read
            .as_ref()
            .map(|read| read.events.as_slice())
            .unwrap_or_default(),
        &round,
    )?;
    let ledger_revision = ledger_high
        .as_ref()
        .map(|(_, payload, _)| payload.ir_revision)
        .unwrap_or(0);
    let persisted_revision = existing.as_ref().map(|value| value.revision).unwrap_or(0);
    let persisted_digest = existing.as_ref().map(validation_digest);
    let persisted_matches_high_water =
        ledger_high
            .as_ref()
            .is_some_and(|(_, payload, production)| {
                *production
                    && payload.ir_revision == persisted_revision
                    && persisted_digest.as_deref() == Some(payload.validation_digest.as_str())
            });
    let document_changed = existing.as_ref().is_none_or(|persisted| {
        serde_json::to_vec(&ir).expect("RoundIr serialization is infallible")
            != serde_json::to_vec(persisted).expect("RoundIr serialization is infallible")
    });
    let budget_only_refresh =
        existing.is_some() && !contract_changed && document_changed && persisted_matches_high_water;
    let ir_written = if existing.is_some() && !document_changed && persisted_matches_high_water {
        false
    } else if budget_only_refresh {
        // Runtime ceilings remain observable/effective without moving the
        // signed revision or its contract digest.
        true
    } else {
        ir.revision = persisted_revision
            .max(ledger_revision)
            .checked_add(1)
            .context("ROUND-IR revision 溢出")?;
        true
    };
    let reverify_tasks = if ir_written {
        card_reverify_tasks
    } else {
        ledger_high
            .as_ref()
            .map(|(_, payload, _)| payload.reverify_tasks.clone())
            .unwrap_or_default()
    };
    let digest = validation_digest(&ir);
    let ir_bytes = if ir_written {
        Some(
            serde_yaml::to_string(&ir)
                .context("序列化 ROUND-IR 失败")?
                .into_bytes(),
        )
    } else {
        None
    };
    let validation_event = if validation_event_exists(root, round, ir.revision, &digest)? {
        None
    } else {
        Some(ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some(round),
            task_validated_payload_with_reverify(ir.revision, &digest, &reverify_tasks),
        ))
    };
    Ok(LegacyPlanArtifact {
        outcome: PlanOutcome {
            ir_path,
            revision: ir.revision,
            digest,
            skipped: ir.skipped.clone(),
            ir_written,
            event_appended: validation_event.is_some(),
            reverify_tasks,
        },
        ir_bytes,
        validation_event,
    })
}

#[cfg(test)]
pub(crate) fn materialize_legacy_plan_fixture(root: &Path) -> Result<PlanOutcome> {
    let round = crate::current_round(root)?;
    let artifact = build_legacy_plan_artifact_readonly(root, &round)?;
    if let Some(bytes) = artifact.ir_bytes.as_deref() {
        atomic_write_round_ir(root, &artifact.outcome.ir_path, bytes)?;
    }
    if let Some(event) = artifact.validation_event.as_ref() {
        ledger::append(root, &round, std::slice::from_ref(event))?;
    }
    Ok(artifact.outcome)
}

fn run_plan_v3_locked(root: &Path, round: &str) -> Result<PlanOutcome> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = read_ledger(&ledger_path).context("读取 schema 3 plan ledger 失败")?;
    if !ledger_read.bad_lines.is_empty() {
        bail!("schema 3 plan 拒绝坏账本");
    }
    if crate::round::contract_schema_from_events(&ledger_read.events, round)?
        != Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        bail!("schema 3 plan 缺 canonical RoundOpened generation marker");
    }
    if orch_core::fold(&ledger_read.events).round_closed {
        bail!("round {round} 已关闭，拒绝 replan");
    }
    let binding_rel = "coordination/PROJECT-BINDING.yaml";
    let binding_bytes = source_file_bytes(root, binding_rel, "schema 3 PROJECT-BINDING")?;
    binding::validate_v3_binding_shape(&binding_bytes)?;
    let binding_yaml =
        std::str::from_utf8(&binding_bytes).context("schema 3 PROJECT-BINDING 必须为 UTF-8")?;
    let admission_binding = binding::parse_binding_bytes(&binding_bytes)
        .map_err(anyhow::Error::msg)
        .context("解析 schema 3 binding admission floor 失败")?;
    binding::validate_rust_gate_floors_at_root(root, &admission_binding).map_err(|errors| {
        anyhow::anyhow!("Rust 门参数 floor 校验失败:\n- {}", errors.join("\n- "))
    })?;

    let loaded = load_task_inputs(root, round, TaskLoadPurpose::AuthorizedPlan)?;
    if loaded.schema_version != ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        bail!("schema 3 plan task loader generation 漂移");
    }
    let plan_binding: PlanBinding =
        serde_yaml::from_str(binding_yaml).context("解析 schema 3 binding workspace 失败")?;
    check_tier_f_workspace(root, &plan_binding.workspace.worktree_root)?;

    let ledger_events = ledger_read.events.as_slice();
    let mut ir = compile_ir_v3(round, binding_yaml, &loaded)?;
    let ir_path = root.join(format!("coordination/rounds/{round}/ROUND-IR.yaml"));
    let ir_rel = ir_path.strip_prefix(root)?.to_string_lossy().to_string();
    let existing = match fs::symlink_metadata(&ir_path) {
        Ok(_) => {
            let bytes = source_file_bytes(root, &ir_rel, "schema 3 ROUND-IR")?;
            let text = std::str::from_utf8(&bytes).context("schema 3 ROUND-IR 必须为 UTF-8")?;
            let parsed = parse_signed_round_ir(text).map_err(anyhow::Error::msg)?;
            if parsed.schema_version != ACTORLESS_ROUND_IR_SCHEMA_VERSION {
                bail!("schema 3 round 不得跨代 replan legacy ROUND-IR");
            }
            Some(parsed)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("读取 schema 3 ROUND-IR 失败"),
    };

    let card_reverify_tasks = existing
        .as_ref()
        .map(|value| {
            tasks_requiring_reverify(
                &value.source_bindings.task_cards,
                &ir.source_bindings.task_cards,
            )
        })
        .unwrap_or_default();
    let contract_changed = reconcile_revision(&mut ir, existing.as_ref())?;
    ensure_replan_safe(existing.is_some() && contract_changed, ledger_events, round)?;
    let ledger_high = validation_high_water(ledger_events, round)?;
    let ledger_revision = ledger_high
        .as_ref()
        .map(|(_, payload, _)| payload.ir_revision)
        .unwrap_or(0);
    let persisted_revision = existing.as_ref().map(|value| value.revision).unwrap_or(0);
    let persisted_digest = existing.as_ref().map(validation_digest);
    let persisted_matches_high_water =
        ledger_high
            .as_ref()
            .is_some_and(|(_, payload, production)| {
                *production
                    && payload.ir_revision == persisted_revision
                    && persisted_digest.as_deref() == Some(payload.validation_digest.as_str())
            });
    let document_changed = existing.as_ref().is_none_or(|persisted| {
        serde_json::to_vec(&ir).expect("schema 3 IR serializes")
            != serde_json::to_vec(persisted).expect("schema 3 persisted IR serializes")
    });
    let ir_written = if existing.is_some() && !document_changed && persisted_matches_high_water {
        false
    } else {
        if contract_changed || !persisted_matches_high_water {
            ir.revision = persisted_revision
                .max(ledger_revision)
                .checked_add(1)
                .context("schema 3 ROUND-IR revision 溢出")?;
        }
        true
    };
    let reverify_tasks = if ir_written {
        card_reverify_tasks
    } else {
        ledger_high
            .as_ref()
            .map(|(_, payload, _)| payload.reverify_tasks.clone())
            .unwrap_or_default()
    };
    let digest = validation_digest(&ir);
    let fresh_ledger = read_ledger(&ledger_path).context("schema 3 plan 写前重读 ledger 失败")?;
    if !fresh_ledger.bad_lines.is_empty()
        || crate::round::contract_schema_from_events(&fresh_ledger.events, round)?
            != Some(ACTORLESS_ROUND_IR_SCHEMA_VERSION)
        || orch_core::fold(&fresh_ledger.events).round_closed
    {
        bail!("schema 3 plan 写前 round ledger 已漂移或关闭");
    }
    if ir_written {
        let yaml = serde_yaml::to_string(&ir).context("序列化 schema 3 ROUND-IR 失败")?;
        atomic_write_round_ir(root, &ir_path, yaml.as_bytes())?;
    }
    let event_appended = if validation_event_exists(root, round, ir.revision, &digest)? {
        false
    } else {
        ledger::append(
            root,
            round,
            &[ledger::event(
                "TaskValidated",
                "runtime:orch",
                None,
                Some(round),
                task_validated_payload_with_reverify(ir.revision, &digest, &reverify_tasks),
            )],
        )?;
        true
    };
    Ok(PlanOutcome {
        ir_path,
        revision: ir.revision,
        digest,
        skipped: Vec::new(),
        ir_written,
        event_appended,
        reverify_tasks,
    })
}

fn bind_mode_source(ir: &mut RoundIr, mode_ref: &str, mode_path: &str) {
    ir.mode_ref = mode_ref.to_string();
    ir.source_bindings.mode_path = mode_path.to_string();
}

fn bind_harness_registry_projection(root: &Path, ir: &mut RoundIr) -> Result<()> {
    let registry = root.join("coordination/harnesses.yaml");
    if !registry.is_file() {
        ir.source_bindings.harness_registry_digest.clear();
        return Ok(());
    }
    ir.source_bindings.harness_registry_digest = crate::harness::registry_digest(root)?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoundModeRef {
    #[serde(rename = "modeRef")]
    mode_ref: String,
}

#[derive(Debug)]
struct SelectedMode {
    path: PathBuf,
    mode_ref: String,
    /// False only for an existing single-mode legacy IR with no binding.
    explicit: bool,
}

fn persisted_mode_ref(root: &Path, round: &str) -> Result<Option<String>> {
    let rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let path = root.join(&rel);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            let bytes = source_file_bytes(root, &rel, "ROUND-IR modeRef")?;
            let value: serde_yaml::Value =
                serde_yaml::from_slice(&bytes).context("解析 ROUND-IR modeRef 失败")?;
            let Some(mode_ref) = value.get("modeRef") else {
                return Ok(None);
            };
            let mode_ref = mode_ref
                .as_str()
                .context("ROUND-IR modeRef 必须为非空字符串")?
                .trim();
            if mode_ref.is_empty() {
                bail!("ROUND-IR modeRef 必须为非空字符串");
            }
            Ok(Some(mode_ref.to_string()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("读取 ROUND-IR 失败: {}", path.display())),
    }
}

fn selected_mode_path(root: &Path, round: &str) -> Result<SelectedMode> {
    let modes_dir = root.join("coordination/modes");
    let mut modes = fs::read_dir(&modes_dir)
        .with_context(|| format!("读取 modes 目录失败: {}", modes_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .filter_map(|path| {
            let mode_ref = path.file_stem()?.to_str()?.to_string();
            Some((mode_ref, path))
        })
        .collect::<Vec<_>>();
    modes.sort_by(|left, right| left.0.cmp(&right.0));
    if modes.is_empty() {
        bail!("coordination/modes 中没有 yaml");
    }
    let available = modes
        .iter()
        .map(|(mode_ref, _)| mode_ref.clone())
        .collect::<Vec<_>>();

    let metadata_rel = format!("coordination/rounds/{round}/MODE-REF.yaml");
    let metadata_path = root.join(&metadata_rel);
    let metadata_ref = match fs::symlink_metadata(&metadata_path) {
        Ok(_) => {
            let bytes = source_file_bytes(root, &metadata_rel, "round modeRef metadata")?;
            let metadata: RoundModeRef =
                serde_yaml::from_slice(&bytes).context("解析 MODE-REF.yaml 失败")?;
            Some(metadata.mode_ref)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("读取 modeRef metadata 失败: {}", metadata_path.display())
            })
        }
    };
    let persisted_ref = persisted_mode_ref(root, round)?;
    if metadata_ref.is_some() && persisted_ref.is_some() && metadata_ref != persisted_ref {
        bail!("MODE-REF.yaml 与 ROUND-IR modeRef 不一致；先统一显式绑定");
    }
    let requested = metadata_ref.or(persisted_ref);
    let explicit = requested.is_some();
    let bound = match requested {
        Some(mode_ref) => mode_ref_binding(Some(&mode_ref), &available),
        None if available.len() == 1 => Ok(available[0].clone()),
        None => mode_ref_binding(None, &available),
    }
    .map_err(anyhow::Error::msg)?;
    let path = modes
        .into_iter()
        .find_map(|(mode_ref, path)| (mode_ref == bound).then_some(path))
        .context("已绑定 modeRef 未解析到 ModeConfig 路径")?;
    Ok(SelectedMode {
        path,
        mode_ref: bound,
        explicit,
    })
}

fn task_ids(root: &Path, round: &str) -> Result<Vec<String>> {
    let tasks_dir = root.join(format!("coordination/rounds/{round}/tasks"));
    let mut ids = fs::read_dir(&tasks_dir)
        .with_context(|| format!("读取 tasks 目录失败: {}", tasks_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    ids.sort();
    Ok(ids)
}

fn check_tier_f_workspace(root: &Path, worktree_root: &str) -> Result<()> {
    validate_source_path(worktree_root, "workspace.worktreeRoot")?;
    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("解析仓库根失败: {}", root.display()))?;
    let workspace = root.join(worktree_root);
    fs::create_dir_all(&workspace)
        .with_context(|| format!("创建 worktreeRoot 失败: {}", workspace.display()))?;
    let canonical_workspace = fs::canonicalize(&workspace)
        .with_context(|| format!("解析 worktreeRoot 失败: {}", workspace.display()))?;
    if !canonical_workspace.starts_with(&canonical_root) {
        bail!("Tier F worktreeRoot 必须位于仓库根内");
    }

    let probe = canonical_workspace.join(format!(".orch-plan-write-probe-{}", std::process::id()));
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .with_context(|| {
            format!(
                "Tier F worktreeRoot 不可写: {}",
                canonical_workspace.display()
            )
        })?;
    drop(file);
    fs::remove_file(&probe)
        .with_context(|| format!("清理 worktreeRoot 写探针失败: {}", probe.display()))?;

    let gitignore = fs::read_to_string(root.join(".gitignore")).context("读取 .gitignore 失败")?;
    for required in [
        "coordination/rounds/*/dispatch/",
        "coordination/runtime/",
        ".worktrees/",
    ] {
        if !gitignore.lines().any(|line| line.trim() == required) {
            bail!("Tier F workspace gitignore 缺少必需行: {required}");
        }
    }
    Ok(())
}

fn atomic_write_round_ir(root: &Path, target: &Path, bytes: &[u8]) -> Result<()> {
    let rel = target
        .strip_prefix(root)
        .context("ROUND-IR target 必须位于仓库 root")?;
    let rel_text = rel
        .to_str()
        .context("ROUND-IR target 必须是 UTF-8 relative path")?;
    validate_source_path(rel_text, "ROUND-IR target")?;

    let parent = target.parent().context("ROUND-IR target 缺 parent")?;
    let parent_rel = parent
        .strip_prefix(root)
        .context("ROUND-IR parent 必须位于仓库 root")?;
    let mut cursor = root.to_path_buf();
    for component in parent_rel.components() {
        let std::path::Component::Normal(name) = component else {
            bail!("ROUND-IR parent 含非 canonical component")
        };
        cursor.push(name);
        let metadata = fs::symlink_metadata(&cursor)
            .with_context(|| format!("ROUND-IR parent stat 失败: {}", cursor.display()))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("ROUND-IR parent 必须是真实目录: {}", cursor.display());
        }
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            bail!("ROUND-IR existing target 必须是 regular non-symlink file")
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("检查 ROUND-IR existing target 失败"),
    }

    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("ROUND-IR target 缺 UTF-8 filename")?;
    let temp = parent.join(format!(
        ".{file_name}.orch-tmp-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("创建 ROUND-IR temp 失败: {}", temp.display()))?;
        file.write_all(bytes).context("写入 ROUND-IR temp 失败")?;
        file.sync_all().context("fsync ROUND-IR temp 失败")?;
        drop(file);

        // Recheck the parent and destination immediately before the only
        // replacement operation.  rename replaces a final symlink itself; it
        // never follows that final component.
        let parent_metadata = fs::symlink_metadata(parent)?;
        if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
            bail!("ROUND-IR parent 在 rename 前发生替换")
        }
        if let Ok(metadata) = fs::symlink_metadata(target) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("ROUND-IR target 在 rename 前变成非 regular file")
            }
        }
        fs::rename(&temp, target).context("原子替换 ROUND-IR 失败")?;
        let directory = File::open(parent).context("打开 ROUND-IR parent 以 fsync 失败")?;
        directory.sync_all().context("fsync ROUND-IR parent 失败")?;

        // A final stable read proves the published path is a regular file and
        // contains exactly the serialized snapshot we intended to publish.
        let published = source_file_bytes(root, rel_text, "ROUND-IR")?;
        if published != bytes {
            bail!("ROUND-IR published bytes 与 candidate 不一致")
        }
        Ok(())
    })();
    if write_result.is_err() {
        match fs::remove_file(&temp) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    write_result
}

fn reconcile_revision(candidate: &mut RoundIr, existing: Option<&RoundIr>) -> Result<bool> {
    let Some(existing) = existing else {
        return Ok(true);
    };
    candidate.revision = existing.revision;
    if validation_digest(candidate) == validation_digest(existing) {
        return Ok(false);
    }
    candidate.revision = existing
        .revision
        .checked_add(1)
        .context("ROUND-IR revision 溢出")?;
    Ok(true)
}

fn validation_event_exists(root: &Path, round: &str, revision: u32, digest: &str) -> Result<bool> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = match read_ledger(&ledger_path) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 TaskValidated 账本失败: {}", ledger_path.display()))
        }
    };
    Ok(
        validation_high_water(&ledger_read.events, round)?.is_some_and(
            |(_, payload, production)| {
                production && payload.ir_revision == revision && payload.validation_digest == digest
            },
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODE: &str = r#"
agents:
  executor: {tier: F}
  verifier: {tier: none, adapter: root-manual}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [secondary-review]}
budgets:
  round: {wallMinutes: 90, maxModelWakes: 12}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;

    const BINDING: &str = r#"
scope: {protectedPaths: ["coordination/**"]}
git: {pushPolicy: forbidden}
verification: {independentVerifier: required-for-write}
"#;

    fn task() -> TaskInput {
        TaskInput {
            id: "T1".into(),
            agent: "executor-desktop".into(),
            seed_protocol: "seeded-red".into(),
            has_seeds: true,
            write_set: vec!["orch/src/lib.rs".into()],
            frozen_paths: vec![],
            gates_fast: vec!["testFast".into()],
            wall_minutes: Some(30),
            required_reviews: vec![card::RequiredReview {
                role: "primary".into(),
                agent: "executor-claw".into(),
            }],
            required_evidence: vec!["contract".into()],
            bootstrap_pre_signoff_attempt: None,
            card_source_sha256: "0".repeat(64),
            seed_source_sha256: BTreeMap::from([(
                "coordination/rounds/r1/seeds/T1/contract.rs".into(),
                "1".repeat(64),
            )]),
        }
    }

    #[test]
    fn seeded_red_without_seed_is_rejected() {
        let mut input = task();
        input.has_seeds = false;
        let error = compile_ir("r1", MODE, BINDING, &[input]).unwrap_err();
        assert!(error.to_string().contains("seed"));
    }

    #[test]
    fn runtime_policy_event_digest_retains_historical_typed_and_map_order() {
        let event: EventRecord=serde_json::from_str(r#"{"eventId":"e","ts":"t","actor":"a","type":"X","payload":{"z":1,"a":{"z":2,"a":3}},"zz":{"z":2,"a":1},"aa":1}"#).unwrap();
        let canonical=runtime_policy_event_bytes(&event).unwrap();
        assert_eq!(String::from_utf8(canonical).unwrap(), r#"{"eventId":"e","ts":"t","actor":"a","type":"X","payload":{"a":{"a":3,"z":2},"z":1},"aa":1,"zz":{"a":1,"z":2}}"#);
        assert_eq!(event.extra["zz"]["z"],2);
    }

    #[test]
    fn r78_revision_five_digest_is_stable_when_review_extensions_are_absent() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("orch-host manifest has repository ancestors");
        let bytes = fs::read(repository.join("coordination/rounds/r78/ROUND-IR.yaml"))
            .expect("r78 revision-five IR is a committed compatibility fixture");
        let ir: RoundIr = serde_yaml::from_slice(&bytes).expect("historical IR still parses");
        assert_eq!(ir.revision, 5);
        assert!(ir.tasks.iter().all(|task| task.review_fallbacks.is_empty()
            && task.review_quorum.is_none()
            && task.nongate_seats.is_empty()));
        assert!(ir.source_bindings.harness_registry_digest.is_empty());
        assert_eq!(
            validation_digest(&ir),
            "be0b602de73cc196b7cb0b4e3f28447331c83bc1948f7736b98604129434a8e4"
        );
    }

    #[test]
    fn review_extensions_are_explicit_reachable_and_closed() {
        let mut ir = compile_ir("r79", MODE, BINDING, &[task()]).unwrap();
        ir.source_bindings.harness_registry_digest = "a".repeat(64);
        ir.scheduling
            .capacities
            .get_mut("executor-opencode")
            .unwrap()
            .roles
            .push("primary-review".to_string());
        ir.scheduling
            .allowed_agents
            .push("executor-dsh".to_string());
        ir.scheduling.capacities.insert(
            "executor-dsh".to_string(),
            IrCapacity {
                agent: 1,
                quota: 1,
                roles: vec!["nongate-review".to_string()],
            },
        );
        let task = &mut ir.tasks[0];
        task.review_fallbacks = vec![card::ReviewFallback {
            role: "primary".to_string(),
            fallback_agent: "executor-opencode".to_string(),
        }];
        task.nongate_seats = vec![card::NongateSeat {
            agent: "executor-dsh".to_string(),
            preset: "minimal".to_string(),
        }];
        task.review_quorum = Some(card::ReviewQuorumPolicy {
            minimum_substantive: 2,
            nongate_may_substitute_failed_formal: true,
            minimum_nongate_pass_for_substitution: 1,
        });
        validate_review_contract_extensions(&ir).unwrap();

        let mut too_small =
            serde_yaml::from_str::<RoundIr>(&serde_yaml::to_string(&ir).unwrap()).unwrap();
        too_small.tasks[0]
            .review_quorum
            .as_mut()
            .unwrap()
            .minimum_substantive = 1;
        assert!(validate_review_contract_extensions(&too_small).is_err());

        let mut overlap =
            serde_yaml::from_str::<RoundIr>(&serde_yaml::to_string(&ir).unwrap()).unwrap();
        overlap.tasks[0].nongate_seats[0].agent = "executor-opencode".to_string();
        assert!(validate_review_contract_extensions(&overlap).is_err());

        let mut unsigned =
            serde_yaml::from_str::<RoundIr>(&serde_yaml::to_string(&ir).unwrap()).unwrap();
        unsigned.tasks[0].review_quorum = None;
        assert!(validate_review_contract_extensions(&unsigned).is_err());
    }

    #[test]
    fn non_forbidden_push_is_rejected() {
        let mode = MODE.replace("pushPolicy: forbidden", "pushPolicy: ask");
        let error = compile_ir("r1", &mode, BINDING, &[task()]).unwrap_err();
        assert!(error.to_string().contains("push"));
    }

    #[test]
    fn changed_ir_increments_revision_while_same_ir_does_not() {
        let existing = compile_ir("r1", MODE, BINDING, &[task()]).unwrap();
        let mut same = compile_ir("r1", MODE, BINDING, &[task()]).unwrap();
        assert!(!reconcile_revision(&mut same, Some(&existing)).unwrap());
        assert_eq!(same.revision, 1);

        let mut changed_task = task();
        changed_task.id = "T2".into();
        let mut changed = compile_ir("r1", MODE, BINDING, &[changed_task]).unwrap();
        assert!(reconcile_revision(&mut changed, Some(&existing)).unwrap());
        assert_eq!(changed.revision, 2);
    }

    #[test]
    fn plan_binds_typed_registry_domains_and_digest_into_the_ir() {
        let root = crate::util::test_scratch_dir("b234-plan-registry-projection");
        fs::create_dir_all(root.join("coordination/tools")).expect("create registry dirs");
        fs::write(
            root.join("coordination/tools/opencode.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: opencode
modelSource: argv
effortSemantic: variant
startupSpacingMs: 1200
maxConcurrent: 3
launch:
  argv: ["opencode", "run", "{message}", "--dir", "{root}", "--model", "{model}", "--variant", "{effort}"]
observation: {source: opencode-db, policy: strict}
"#,
        )
        .expect("write ToolDefinition");
        fs::write(
            root.join("coordination/agents.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-opencode:
    tool: opencode
    model: provider/model-a
    effort: high
    roles: [implement]
    responsibility: implementation
    injectable: true
    sessionId: fresh-session-per-wake
  executor-opencode-scout:
    tool: opencode
    model: provider/model-b
    effort: minimal
    roles: [primary-review]
    responsibility: exploration
    injectable: true
    sessionId: fresh-session-per-wake
"#,
        )
        .expect("write AgentRegistry");

        let mode = r#"
agents:
  executor: {tier: F}
  verifier: {tier: none, adapter: root-manual}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-opencode, executor-opencode-scout]
  capacities:
    executor-opencode: {agent: 1, quota: 1, roles: [implement]}
    executor-opencode-scout: {agent: 1, quota: 1, roles: [primary-review]}
  quotaDomains:
    opencode: {agent: 1, quota: 1}
budgets:
  round: {wallMinutes: 90, maxModelWakes: 12}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#;
        let mut input = task();
        input.agent = "executor-opencode".into();
        input.required_reviews[0].agent = "executor-opencode-scout".into();
        let mut ir = compile_ir("rB234", mode, BINDING, &[input]).expect("compile candidate IR");

        bind_agent_registry_projection(&root, &mut ir).expect("bind registry projection");

        assert_eq!(
            ir.scheduling.agent_domains,
            BTreeMap::from([
                ("executor-opencode".into(), "opencode".into()),
                ("executor-opencode-scout".into(), "opencode".into()),
            ])
        );
        assert_eq!(
            ir.scheduling.quota_domains.get("opencode"),
            Some(&IrQuotaDomain { agent: 1, quota: 1 })
        );
        let digest = &ir.source_bindings.agent_registry_digest;
        assert_eq!(digest.len(), 64, "{digest}");
        assert!(
            digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{digest}"
        );
        let serialized = serde_yaml::to_string(&ir).expect("serialize bound IR");
        assert!(serialized.contains("agentRegistryDigest:"), "{serialized}");
        assert!(serialized.contains("agentDomains:"), "{serialized}");
        assert!(serialized.contains("quotaDomains:"), "{serialized}");
    }

    const H142_ROUND: &str = "rH142";

    fn h142_event(
        event_id: &str,
        kind: &str,
        actor: &str,
        task_id: &str,
        payload: serde_json::Value,
    ) -> EventRecord {
        EventRecord {
            event_id: event_id.to_string(),
            ts: "2026-08-10T00:00:00Z".to_string(),
            actor: actor.to_string(),
            kind: kind.to_string(),
            task_id: Some(task_id.to_string()),
            round: Some(H142_ROUND.to_string()),
            payload: Some(payload),
            extra: serde_json::Map::new(),
        }
    }

    fn h142_dispatch(task_id: &str, attempt_id: &str) -> EventRecord {
        h142_event(
            &format!("dispatch-{attempt_id}"),
            "DispatchIssued",
            "runtime:orch",
            task_id,
            serde_json::json!({
                "attemptId": attempt_id,
                "agent": "executor-desktop"
            }),
        )
    }

    #[test]
    fn h142_root_fail_releases_attempt_for_replan() {
        let events = [
            h142_dispatch("T1", "T1-A0001"),
            h142_event(
                "root-fail",
                "VerdictIssued",
                "verifier:root",
                "T1",
                serde_json::json!({
                    "attemptId": "T1-A0001",
                    "verdict": "FAIL"
                }),
            ),
        ];

        ensure_replan_safe(true, &events, H142_ROUND).expect("root FAIL must release the attempt");
    }

    #[test]
    fn h142_root_blocked_releases_attempt() {
        let events = [
            h142_dispatch("T1", "T1-A0001"),
            h142_event(
                "root-blocked",
                "VerdictIssued",
                "verifier:root",
                "T1",
                serde_json::json!({
                    "attemptId": "T1-A0001",
                    "verdict": "BLOCKED"
                }),
            ),
        ];

        assert!(replan_invalidation_from_events(&events, H142_ROUND)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn h142_root_pass_keeps_attempt_live() {
        let events = [
            h142_dispatch("T1", "T1-A0001"),
            h142_event(
                "root-pass",
                "VerdictIssued",
                "verifier:root",
                "T1",
                serde_json::json!({
                    "attemptId": "T1-A0001",
                    "verdict": "PASS"
                }),
            ),
        ];

        let message = ensure_replan_safe(true, &events, H142_ROUND)
            .unwrap_err()
            .to_string();
        assert!(message.contains("taskId=T1"), "{message}");
        assert!(message.contains("attemptId=T1-A0001"), "{message}");
        assert!(message.contains("state=VerdictIssued"), "{message}");
    }

    #[test]
    fn h142_non_root_fail_keeps_attempt_live() {
        let events = [
            h142_dispatch("T1", "T1-A0001"),
            h142_event(
                "forged-fail",
                "VerdictIssued",
                "verifier:peer",
                "T1",
                serde_json::json!({
                    "attemptId": "T1-A0001",
                    "verdict": "FAIL"
                }),
            ),
        ];

        let message = ensure_replan_safe(true, &events, H142_ROUND)
            .unwrap_err()
            .to_string();
        assert!(message.contains("attemptId=T1-A0001"), "{message}");
        assert!(message.contains("state=VerdictIssued"), "{message}");
    }

    #[test]
    fn h142_unknown_or_missing_verdict_keeps_attempt_live() {
        for (event_id, payload) in [
            (
                "unknown-verdict",
                serde_json::json!({
                    "attemptId": "T1-A0001",
                    "verdict": "UNKNOWN"
                }),
            ),
            (
                "missing-verdict",
                serde_json::json!({
                    "attemptId": "T1-A0001"
                }),
            ),
        ] {
            let events = [
                h142_dispatch("T1", "T1-A0001"),
                h142_event(event_id, "VerdictIssued", "verifier:root", "T1", payload),
            ];

            let live = replan_invalidation_from_events(&events, H142_ROUND).unwrap();
            assert_eq!(live.len(), 1, "{event_id}");
            assert_eq!(live[0].state, "VerdictIssued", "{event_id}");
        }
    }

    #[test]
    fn h142_terminal_root_verdict_requires_nonempty_attempt_id() {
        for (event_id, payload) in [
            ("missing-attempt", serde_json::json!({"verdict": "FAIL"})),
            (
                "empty-attempt",
                serde_json::json!({
                    "attemptId": "",
                    "verdict": "FAIL"
                }),
            ),
        ] {
            let events = [
                h142_dispatch("T1", "T1-A0001"),
                h142_event(event_id, "VerdictIssued", "verifier:root", "T1", payload),
            ];

            let message = replan_invalidation_from_events(&events, H142_ROUND)
                .unwrap_err()
                .to_string();
            assert!(message.contains("缺非空 attemptId"), "{message}");
        }
    }

    #[test]
    fn h142_unknown_attempt_id_is_noop() {
        let dispatch = h142_dispatch("T1", "T1-A0001");
        let before =
            replan_invalidation_from_events(std::slice::from_ref(&dispatch), H142_ROUND).unwrap();

        let events = [
            dispatch,
            h142_event(
                "unknown-attempt",
                "VerdictIssued",
                "verifier:root",
                "T1",
                serde_json::json!({
                    "attemptId": "T1-A9999",
                    "verdict": "FAIL"
                }),
            ),
        ];
        let after = replan_invalidation_from_events(&events, H142_ROUND).unwrap();

        assert_eq!(after, before);
    }

    #[test]
    fn h142_b254_shape_clears_both_attempts() {
        let events = [
            h142_dispatch("B254", "B254-A0001"),
            h142_event(
                "b254-root-fail",
                "VerdictIssued",
                "verifier:root",
                "B254",
                serde_json::json!({
                    "attemptId": "B254-A0001",
                    "verdict": "FAIL"
                }),
            ),
            h142_dispatch("B254", "B254-A0002"),
            h142_event(
                "b254-a2-blocked",
                "AttemptBlocked",
                "runtime:orch",
                "B254",
                serde_json::json!({
                    "attemptId": "B254-A0002"
                }),
            ),
        ];

        ensure_replan_safe(true, &events, H142_ROUND)
            .expect("B254 replay must leave no live attempt");
    }

    #[test]
    fn h142_task_recorded_clears_pass_verdicted_attempt() {
        let pass_events = [
            h142_dispatch("T1", "T1-A0001"),
            h142_event(
                "root-pass-before-record",
                "VerdictIssued",
                "verifier:root",
                "T1",
                serde_json::json!({
                    "attemptId": "T1-A0001",
                    "verdict": "PASS"
                }),
            ),
        ];
        assert_eq!(
            replan_invalidation_from_events(&pass_events, H142_ROUND)
                .unwrap()
                .len(),
            1,
            "PASS must remain live until TaskRecorded"
        );

        let events = [
            pass_events[0].clone(),
            pass_events[1].clone(),
            h142_event(
                "task-recorded",
                "TaskRecorded",
                "runtime:orch",
                "T1",
                serde_json::json!({}),
            ),
        ];
        assert!(replan_invalidation_from_events(&events, H142_ROUND)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn budget_refresh_keeps_signed_revision_while_real_contract_change_is_guarded() {
        let existing = compile_ir("r1", MODE, BINDING, &[task()]).unwrap();
        let raised_budget = MODE.replace("maxModelWakes: 12", "maxModelWakes: 20");
        let mut budget_candidate = compile_ir("r1", &raised_budget, BINDING, &[task()]).unwrap();
        assert!(!reconcile_revision(&mut budget_candidate, Some(&existing)).unwrap());
        assert_eq!(budget_candidate.revision, existing.revision);
        assert_eq!(
            validation_digest(&budget_candidate),
            validation_digest(&existing)
        );
        assert_ne!(
            serde_json::to_vec(&budget_candidate).unwrap(),
            serde_json::to_vec(&existing).unwrap(),
            "the effective/observable runtime ceiling still refreshes"
        );

        let dispatch: EventRecord = serde_json::from_value(serde_json::json!({
            "eventId": "dispatch-t1",
            "ts": "2026-07-28T00:00:00Z",
            "actor": "runtime:orch",
            "type": "DispatchIssued",
            "taskId": "T1",
            "round": "r1",
            "payload": {
                "attemptId": "T1-A0001",
                "agent": "executor-desktop"
            }
        }))
        .unwrap();
        ensure_replan_safe(false, std::slice::from_ref(&dispatch), "r1").unwrap();
        let error = ensure_replan_safe(true, &[dispatch], "r1").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("T1-A0001"));
        assert!(message.contains("executor-desktop"));
        assert!(message.contains("DispatchIssued"));
    }

    #[test]
    fn tightened_scheduling_checks_only_undispatched_tasks() {
        let tightened = MODE
            .replace(
                "allowedAgents: [executor-desktop, executor-claw, executor-opencode]",
                "allowedAgents: [executor-claw, executor-opencode]",
            )
            .replace(
                "    executor-desktop: {agent: 2, quota: 2, roles: [implement]}\n",
                "",
            );
        let undispatched =
            compile_ir_with_dispatched("r1", &tightened, BINDING, &[task()], &BTreeSet::new())
                .unwrap_err();
        assert!(undispatched.to_string().contains("未获 scheduling 授权"));

        let dispatched = BTreeSet::from(["T1".to_string()]);
        compile_ir_with_dispatched("r1", &tightened, BINDING, &[task()], &dispatched)
            .expect("rules already bound when T1 was dispatched");
    }

    /// B147：构造空元数据的 LoadedTasks 侧车（digest 语义与 compile 直出等价）。
    fn empty_loaded(tasks: Vec<TaskInput>) -> LoadedTasks {
        let task_order = tasks.iter().map(|task| task.id.clone()).collect();
        LoadedTasks {
            schema_version: SCHEDULED_ROUND_IR_SCHEMA_VERSION,
            tasks,
            task_order,
            entry_points_by_task: BTreeMap::new(),
            capabilities_by_task: BTreeMap::new(),
            depends_on_by_task: BTreeMap::new(),
            complexity_by_task: BTreeMap::new(),
            review_fallbacks_by_task: BTreeMap::new(),
            nongate_seats_by_task: BTreeMap::new(),
            review_quorum_by_task: BTreeMap::new(),
            primary_pass_alone_satisfies_by_task: BTreeMap::new(),
        }
    }

    #[test]
    fn production_plan_aggregates_locked_and_all_targets_floors() {
        let module = "orch/crates/orch-host/src/fresh.rs";
        let root = b170_module_site(
            "rust-floor",
            "B170F",
            &[module],
            &["coordination/**"],
            &[module],
            "pub mod fresh;\npub mod plan;\n",
            &[],
            &[],
        );
        fs::write(root.join("orch/Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            "project: {ecosystems: [rust]}\n\
             commands:\n\
               testFast: {argv: [cargo, test, --workspace]}\n\
               check: {argv: [cargo, check, --workspace]}\n\
             scope: {protectedPaths: [\"coordination/**\"]}\n\
             git: {pushPolicy: forbidden}\n\
             verification: {independentVerifier: required-for-write}\n",
        )
        .unwrap();
        let error = build_legacy_plan_artifact_readonly(&root, "r57")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("check") && error.contains("--locked"),
            "{error}"
        );
        assert!(
            error.contains("check") && error.contains("--all-targets"),
            "{error}"
        );
        assert!(
            !root.join("coordination/rounds/r57/ROUND-IR.yaml").exists(),
            "gate floor refusal must precede ROUND-IR persistence"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn readonly_ir_validation_accepts_same_semantics_and_yaml_reformatting() {
        let persisted = compile_ir("r1", MODE, BINDING, &[task()]).unwrap();
        let yaml = serde_yaml::to_string(&persisted).unwrap();
        let exact = compare_candidate_with_persisted(
            "r1",
            MODE,
            BINDING,
            &[task()],
            &yaml,
            &empty_loaded(vec![task()]),
            None,
        )
        .unwrap();
        assert!(exact.digest_matches);
        assert_eq!(exact.persisted_revision, 1);

        // JSON 是 YAML 的合法子集；只改变表现形式不得制造 IR 漂移。
        let reformatted = serde_json::to_string_pretty(&persisted).unwrap();
        let reformatted_check = compare_candidate_with_persisted(
            "r1",
            MODE,
            BINDING,
            &[task()],
            &reformatted,
            &empty_loaded(vec![task()]),
            None,
        )
        .unwrap();
        assert!(reformatted_check.digest_matches);
    }

    #[test]
    fn readonly_ir_validation_rejects_card_mode_and_binding_semantic_drift() {
        let persisted = compile_ir("r1", MODE, BINDING, &[task()]).unwrap();
        let yaml = serde_yaml::to_string(&persisted).unwrap();

        let mut changed_card = task();
        changed_card.write_set = vec!["orch/src/changed.rs".into()];
        assert!(
            !compare_candidate_with_persisted(
                "r1",
                MODE,
                BINDING,
                &[changed_card],
                &yaml,
                &empty_loaded(vec![task()]),
                None,
            )
            .unwrap()
            .digest_matches
        );

        let changed_budget = MODE.replace("wallMinutes: 90", "wallMinutes: 91");
        assert!(
            compare_candidate_with_persisted(
                "r1",
                &changed_budget,
                BINDING,
                &[task()],
                &yaml,
                &empty_loaded(vec![task()]),
                None,
            )
            .unwrap()
            .digest_matches
        );
        let changed_mode = MODE.replace(
            "roles: [implement]",
            "roles: [implement, critical-implement]",
        );
        assert!(
            !compare_candidate_with_persisted(
                "r1",
                &changed_mode,
                BINDING,
                &[task()],
                &yaml,
                &empty_loaded(vec![task()]),
                None,
            )
            .unwrap()
            .digest_matches
        );

        let changed_binding = BINDING.replace(
            "protectedPaths: [\"coordination/**\"]",
            "protectedPaths: [\"orch/**\"]",
        );
        assert!(compare_candidate_with_persisted(
            "r1",
            MODE,
            &changed_binding,
            &[task()],
            &yaml,
            &empty_loaded(vec![task()]),
            None,
        )
        .is_err());
    }

    #[test]
    fn missing_task_bound_is_rejected() {
        let mut input = task();
        input.wall_minutes = None;
        let error = compile_ir("r1", MODE, BINDING, &[input]).unwrap_err();
        assert!(error.to_string().contains("bound"));
    }

    #[test]
    fn broad_writeset_rejects_exact_protected_path() {
        let binding = BINDING.replace(
            "protectedPaths: [\"coordination/**\"]",
            "protectedPaths: [\"coordination/BOARD.md\"]",
        );
        let mut input = task();
        input.write_set = vec!["coordination/**".into()];
        let error = compile_ir("r1", MODE, &binding, &[input]).unwrap_err();
        assert!(error.to_string().contains("protected"));
    }

    #[test]
    fn validation_high_water_forbids_legacy_after_first_production() {
        let event = |revision, digest: &str| {
            ledger::event(
                "TaskValidated",
                "runtime:orch",
                None,
                Some("r1"),
                task_validated_payload(revision, digest),
            )
        };
        let events = vec![
            event(1, &"a".repeat(64)),
            event(2, "0123456789abcdef"),
            event(3, &"b".repeat(64)),
        ];
        let error = validation_high_water(&events, "r1")
            .unwrap_err()
            .to_string();
        assert!(error.contains("降级"), "{error}");
    }

    #[test]
    fn signed_hierarchy_order_is_data_driven() {
        let reordered = MODE.replace(
            "[executor-desktop, executor-claw, executor-opencode]",
            "[executor-opencode, executor-desktop, executor-claw]",
        );
        let ir = compile_ir("r1", &reordered, BINDING, &[task()]).unwrap();
        assert_eq!(
            ir.scheduling.allowed_agents,
            vec!["executor-opencode", "executor-desktop", "executor-claw"]
        );
    }

    #[test]
    fn mode_selection_refuses_implicit_choice_and_unknown_ref() {
        let root = crate::util::test_scratch_dir("b150-mode-ref");
        fs::create_dir_all(root.join("coordination/modes")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
        fs::write(
            root.join("coordination/modes/z-stable.yaml"),
            "kind: ModeConfig\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/modes/aaa-experimental.yaml"),
            "kind: ModeConfig\n",
        )
        .unwrap();

        let missing = selected_mode_path(&root, "r1").unwrap_err().to_string();
        assert!(missing.contains("modeRef 缺失"), "{missing}");

        fs::write(
            root.join("coordination/rounds/r1/MODE-REF.yaml"),
            "modeRef: ghost\n",
        )
        .unwrap();
        let unknown = selected_mode_path(&root, "r1").unwrap_err().to_string();
        assert!(unknown.contains("不在可用"), "{unknown}");

        fs::write(
            root.join("coordination/rounds/r1/MODE-REF.yaml"),
            "modeRef: z-stable\n",
        )
        .unwrap();
        let selected = selected_mode_path(&root, "r1").unwrap();
        assert_eq!(selected.mode_ref, "z-stable");
        assert!(selected.explicit);
        assert_eq!(selected.path, root.join("coordination/modes/z-stable.yaml"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_single_mode_is_readable_but_planning_can_bind_it() {
        let root = crate::util::test_scratch_dir("b150-legacy-mode");
        fs::create_dir_all(root.join("coordination/modes")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
        fs::write(
            root.join("coordination/modes/relay-selfhost.yaml"),
            "kind: ModeConfig\n",
        )
        .unwrap();

        let selected = selected_mode_path(&root, "r1").unwrap();
        assert_eq!(selected.mode_ref, "relay-selfhost");
        assert!(!selected.explicit);
        fs::remove_dir_all(root).unwrap();
    }

    // --- B170 · H32 crate-root module reachability guard ---

    #[test]
    fn module_declaration_scan_ignores_commented_out_modules() {
        let declarations = public_module_declarations(
            "/* pub mod ghost;\n/* pub mod nested; */\n*/\n\
             // pub mod line_comment;\n\
             pub mod live; // retained\n",
        );
        assert_eq!(declarations, vec!["live"]);
    }

    fn b170_module_site(
        tag: &str,
        task_id: &str,
        write_set: &[&str],
        frozen_paths: &[&str],
        entry_points: &[&str],
        lib_source: &str,
        head_files: &[(&str, &str)],
        worktree_only_files: &[(&str, &str)],
    ) -> PathBuf {
        use std::process::Command;

        let root = crate::util::test_scratch_dir(&format!("b170-module-{tag}"));
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C"])
                .arg(root.to_str().unwrap())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?}", args);
        };
        let write_file = |relative: &str, contents: &str| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        };

        run(&["init", "-q"]);
        run(&["config", "user.name", "orch test"]);
        run(&["config", "user.email", "orch-test@example.invalid"]);
        for path in [
            "coordination/runtime",
            "coordination/modes",
            "coordination/rounds/r57/tasks",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        write_file(
            ".gitignore",
            ".worktrees/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\n",
        );
        write_file("coordination/runtime/CURRENT-ROUND", "r57\n");
        write_file("coordination/modes/test.yaml", MODE);
        write_file("coordination/PROJECT-BINDING.yaml", BINDING);
        write_file("orch/crates/orch-host/src/lib.rs", lib_source);
        for (relative, contents) in head_files {
            write_file(relative, contents);
        }

        let card = format!(
            "---\n\
             taskId: {task_id}\n\
             round: r57\n\
             agent: executor-desktop\n\
             seedProtocol: pure-spec\n\
             entryPoints: {}\n\
             writeSet: {}\n\
             frozenPaths: {}\n\
             gates: {{fast: [testFast]}}\n\
             budgets: {{wallMinutes: 30}}\n\
             requiredReviews:\n\
             - {{role: primary, agent: executor-claw}}\n\
             requiredEvidence: [module-reachability]\n\
             ---\n\
             # module reachability fixture\n",
            serde_json::to_string(entry_points).unwrap(),
            serde_json::to_string(write_set).unwrap(),
            serde_json::to_string(frozen_paths).unwrap(),
        );
        write_file(
            &format!("coordination/rounds/r57/tasks/{task_id}.md"),
            &card,
        );
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "fixture"]);
        run(&["branch", "-M", "main"]);

        // Deliberately created only after the commit: production validation
        // must consult HEAD, never Path::exists() on this mutable worktree.
        for (relative, contents) in worktree_only_files {
            write_file(relative, contents);
        }
        root
    }

    #[test]
    fn run_plan_accepts_a_head_declared_new_module() {
        let module = "orch/crates/orch-host/src/fresh.rs";
        let root = b170_module_site(
            "declared",
            "B170D",
            &[module],
            &["coordination/**"],
            &[module],
            "pub mod fresh;\npub mod plan;\n",
            &[],
            &[],
        );
        let outcome = materialize_legacy_plan_fixture(&root)
            .expect("HEAD declaration makes the new module reachable");
        assert!(outcome.ir_written || outcome.event_appended);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_plan_artifact_builder_is_filesystem_read_only() {
        let module = "orch/crates/orch-host/src/fresh.rs";
        let root = b170_module_site(
            "readonly",
            "B170R",
            &[module],
            &["coordination/**"],
            &[module],
            "pub mod fresh;\npub mod plan;\n",
            &[],
            &[],
        );
        let ledger_path = root.join("coordination/rounds/r57/events.jsonl");
        let before_ledger = fs::read(&ledger_path).ok();
        assert!(!root.join(".worktrees").exists());
        assert!(!root
            .join("coordination/rounds/r57/ROUND-IR.yaml")
            .exists());
        let artifact = build_legacy_plan_artifact_readonly(&root, "r57").unwrap();
        assert!(artifact.ir_bytes.is_some());
        assert!(artifact.validation_event.is_some());
        assert!(!root.join(".worktrees").exists());
        assert_eq!(fs::read(&ledger_path).ok(), before_ledger);
        assert!(!root
            .join("coordination/rounds/r57/ROUND-IR.yaml")
            .exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_plan_accepts_a_new_module_when_the_card_owns_lib_rs() {
        let module = "orch/crates/orch-host/src/fresh.rs";
        let lib = "orch/crates/orch-host/src/lib.rs";
        let root = b170_module_site(
            "writable-lib",
            "B170W",
            &[module, lib],
            &["coordination/**"],
            &[module, lib],
            "pub mod plan;\n",
            &[],
            &[],
        );
        let outcome = materialize_legacy_plan_fixture(&root)
            .expect("a card that owns lib.rs can add the declaration");
        assert!(outcome.ir_written || outcome.event_appended);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_plan_replays_b162_and_ignores_an_untracked_module_leftover() {
        let hooks = "orch/crates/orch-host/src/hooks.rs";
        let lib = "orch/crates/orch-host/src/lib.rs";
        let root = b170_module_site(
            "b162-replay",
            "B162",
            &[
                hooks,
                "orch/crates/orch-host/src/round.rs",
                "orch/crates/orch-host/tests/main_guard.rs",
                ".githooks/reference-transaction",
            ],
            &["coordination/**", lib],
            &[hooks, "orch/crates/orch-host/src/round.rs"],
            "pub mod plan;\npub mod round;\n",
            &[("orch/crates/orch-host/src/round.rs", "// existing\n")],
            &[(hooks, "// untracked leftover must not count\n")],
        );
        let error = materialize_legacy_plan_fixture(&root)
            .expect_err("the original B162 authorization shape must fail at plan time")
            .to_string();
        for expected in ["B162", hooks, lib, "pub mod hooks;", "planner", "writeSet"] {
            assert!(error.contains(expected), "missing {expected:?}: {error}");
        }
        assert!(
            !root.join("coordination/rounds/r57/ROUND-IR.yaml").exists(),
            "refusal must precede ROUND-IR persistence"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_plan_never_flags_cargo_discovered_files() {
        let test = "orch/crates/orch-host/tests/new_contract.rs";
        let build = "orch/crates/orch-host/build.rs";
        let main = "orch/crates/orch-host/src/main.rs";
        let root = b170_module_site(
            "cargo-discovered",
            "B170C",
            &[test, build, main],
            &["coordination/**"],
            &[test, build, main],
            "pub mod plan;\n",
            &[],
            &[],
        );
        let outcome =
            materialize_legacy_plan_fixture(&root)
                .expect("tests/*.rs, build.rs, and src/main.rs need no lib declaration");
        assert!(outcome.ir_written || outcome.event_appended);
        let _ = fs::remove_dir_all(root);
    }

    // --- B154 · H20 same-IR-file plan-time guard (integration) ---

    /// Two cards that both write `orch/crates/orch-host/src/plan.rs` (an
    /// IR-structure file) with no declared `dependsOn` must make `run_plan`
    /// refuse the whole round before it produces a new revision; adding a
    /// `dependsOn` edge serializes the pair and lets it through.
    fn b154_same_ir_site(tag: &str, t2_depends_on: &str) -> PathBuf {
        use std::process::Command;
        let root = crate::util::test_scratch_dir(&format!("b154-ir-conflict-{tag}"));
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(["-C"])
                .arg(root.to_str().unwrap())
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?}", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "orch test"]);
        run(&["config", "user.email", "orch-test@example.invalid"]);
        for path in [
            "coordination/runtime",
            "coordination/modes",
            "coordination/rounds/r49/tasks",
            "coordination/rounds/r49/seeds/T1",
            "coordination/rounds/r49/seeds/T2",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\n",
        )
        .unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r49\n").unwrap();
        fs::write(root.join("coordination/modes/test.yaml"), MODE).unwrap();
        fs::write(root.join("coordination/PROJECT-BINDING.yaml"), BINDING).unwrap();
        let seed = b"#[test]\nfn contract() {}\n";
        let seed_sha = hex::encode(sha2::Sha256::digest(seed));
        for task in ["T1", "T2"] {
            fs::write(
                root.join(format!("coordination/rounds/r49/seeds/{task}/contract.rs")),
                seed,
            )
            .unwrap();
        }
        // Both writeSet-s include plan.rs (an IR-structure file) plus their own
        // test target; frozenPaths stay disjoint from the writes.
        let plan_rel = "orch/crates/orch-host/src/plan.rs";
        for (task, deps_field) in [("T1", "dependsOn: []"), ("T2", t2_depends_on)] {
            let card = format!(
                "---\n\
                 taskId: {task}\n\
                 round: r49\n\
                 agent: executor-desktop\n\
                 seedProtocol: seeded-red\n\
                 seeds:\n\
                 - {{src: coordination/rounds/r49/seeds/{task}/contract.rs, target: tests/{task}.rs, sha256: {seed_sha}}}\n\
                 writeSet: [\"{plan_rel}\", \"tests/{task}.rs\"]\n\
                 frozenPaths: [\"coordination/**\"]\n\
                 {deps_field}\n\
                 gates: {{fast: [testFast]}}\n\
                 budgets: {{wallMinutes: 30}}\n\
                 requiredReviews:\n\
                 - {{role: primary, agent: executor-claw}}\n\
                 requiredEvidence: [same-ir-pairs-need-dependson]\n\
                 ---\n\
                 # {task} writes plan.rs\n"
            );
            fs::write(
                root.join(format!("coordination/rounds/r49/tasks/{task}.md")),
                card,
            )
            .unwrap();
        }
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "fixture"]);
        run(&["branch", "-M", "main"]);
        root
    }

    #[test]
    fn same_ir_file_conflict_rejects_undeclared_pair_at_plan() {
        let root = b154_same_ir_site("reject", "dependsOn: []");
        let error = build_legacy_plan_artifact_readonly(&root, "r49")
            .unwrap_err()
            .to_string();
        assert!(error.contains("plan.rs"), "{error}");
        assert!(error.contains("T1") && error.contains("T2"), "{error}");
        assert!(error.contains("dependsOn"), "{error}");
        // plan refused ⇒ no ROUND-IR and no ledger write.
        assert!(
            !root.join("coordination/rounds/r49/ROUND-IR.yaml").exists(),
            "plan should not have persisted an IR on refusal"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn same_ir_file_conflict_accepts_declared_dependency_pair_at_plan() {
        let root = b154_same_ir_site("accept", "dependsOn: [T1]");
        let outcome = materialize_legacy_plan_fixture(&root).unwrap();
        assert!(outcome.ir_written || outcome.event_appended);
        assert!(
            root.join("coordination/rounds/r49/ROUND-IR.yaml").exists(),
            "plan should have persisted an IR once the pair is serialized"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn same_ir_file_conflict_passes_when_only_one_card_writes_the_ir_file() {
        // Sanity: a single writer of an IR-structure file is never blocked.
        let root = b154_same_ir_site("single", "dependsOn: []");
        // Rewrite T2 to NOT write plan.rs — only its own test target.
        fs::write(
            root.join("coordination/rounds/r49/tasks/T2.md"),
            fs::read_to_string(root.join("coordination/rounds/r49/tasks/T2.md"))
                .unwrap()
                .replace(
                    "writeSet: [\"orch/crates/orch-host/src/plan.rs\", \"tests/T2.rs\"]",
                    "writeSet: [\"tests/T2.rs\"]",
                ),
        )
        .unwrap();
        let _ = std::process::Command::new("git")
            .args(["-C"])
            .arg(root.to_str().unwrap())
            .args(["add", "."])
            .status();
        let _ = std::process::Command::new("git")
            .args(["-C"])
            .arg(root.to_str().unwrap())
            .args(["commit", "-q", "-m", "t2 drops plan.rs"])
            .status();
        let outcome = materialize_legacy_plan_fixture(&root).unwrap();
        assert!(outcome.ir_written || outcome.event_appended);
        let _ = fs::remove_dir_all(&root);
    }
}

/// Dependency-specific dispatch decision.  An unfinished dependency and a
/// completed dependency that is absent from the attempt baseline require
/// different operator actions, so they remain distinct typed outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyAdmission {
    /// Every predecessor is recorded and included in the optional baseline.
    Admitted,
    /// At least one declared predecessor has not been recorded.
    BlockedByDependency {
        /// Unfinished predecessor identities in declared order.
        blockers: Vec<String>,
    },
    /// The predecessor is recorded but absent from this attempt baseline.
    ForwardBaselineRequired {
        /// Recorded predecessor missing from the baseline.
        dependency: String,
        /// Recorded predecessor merge that the baseline must contain.
        merge_sha: String,
        /// Concrete baseline that failed the ancestry check.
        attempt_base_sha: String,
    },
}

/// Decide whether a task's declared dependencies admit one concrete attempt
/// baseline.  Missing dependencies take precedence over stale-baseline
/// repair so callers never conflate "not finished" with "finished, but this
/// tree is old".  Both dependency order and duplicate declarations are kept.
pub fn dependency_dispatch_admission(
    depends_on: &[String],
    recorded_with_merge: &[(String, String)],
    attempt_base_sha: Option<&str>,
    base_contains: &dyn Fn(&str) -> bool,
) -> DependencyAdmission {
    let recorded = recorded_with_merge
        .iter()
        .map(|(task, _)| task.clone())
        .collect::<Vec<_>>();
    let blockers = crate::plan::task_dependency_blockers(depends_on, &recorded);
    if !blockers.is_empty() {
        return DependencyAdmission::BlockedByDependency { blockers };
    }

    let Some(attempt_base_sha) = attempt_base_sha else {
        return DependencyAdmission::Admitted;
    };
    for dependency in depends_on {
        if let Some((_, merge_sha)) = recorded_with_merge
            .iter()
            .find(|(task, _)| task == dependency)
        {
            if !base_contains(merge_sha) {
                return DependencyAdmission::ForwardBaselineRequired {
                    dependency: dependency.clone(),
                    merge_sha: merge_sha.clone(),
                    attempt_base_sha: attempt_base_sha.to_string(),
                };
            }
        }
    }

    DependencyAdmission::Admitted
}

