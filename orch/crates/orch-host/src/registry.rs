//! Agent/tool registry v1 (B233).
//!
//! The legacy wake schema remains owned by `wake.rs`.  This module adds the
//! typed tool/model layer around it without changing the frozen `WakeSpec` or
//! `AgentProfile` shapes.

//! Retired registry writers cannot be imported, even in a selfhost build.
//! ```compile_fail
//! use orch_host::registry::amend_agent_pin;
//! ```
//! ```compile_fail
//! use orch_host::registry::run_agent_pin_amendment;
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use orch_core::EventRecord;
use serde::{Deserialize, Serialize};

use crate::agent_profile::{validate_profiles, AgentProfile, QualityClass};
use crate::wake::{render_wake_argv, WakeSpec};

/// Durable ledger fact emitted after an audited agent pin amendment commits.
pub const AGENT_PIN_AMENDED_EVENT_KIND: &str = "AgentPinAmended";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentPin {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// Historical provider/model/effort amendment used by fixed-round readers.
/// New invocation pins belong to the local harness snapshot; this module no
/// longer writes registry amendments or emits AgentPinAmended events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPinAmendment {
    agent: String,
    before: AgentPin,
    after: AgentPin,
    registry_digest_before: String,
    registry_digest_after: String,
    round: String,
    ir_revision: u32,
    actor: String,
    reason: String,
}

impl AgentPinAmendment {
    pub(crate) fn ir_revision(&self) -> u32 {
        self.ir_revision
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Argv,
    Env,
    ExternalConfig,
}

impl<'de> Deserialize<'de> for ModelSource {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "argv" => Ok(Self::Argv),
            "env" => Ok(Self::Env),
            "external-config" => Ok(Self::ExternalConfig),
            other => Err(serde::de::Error::custom(format!(
                "unknown modelSource {other:?}; expected argv/env/external-config"
            ))),
        }
    }
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Argv => "argv",
            Self::Env => "env",
            Self::ExternalConfig => "external-config",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub tool: String,
    #[serde(rename = "modelSource")]
    pub model_source: ModelSource,
    #[serde(default)]
    #[serde(rename = "effortSemantic")]
    pub effort_semantic: Option<String>,
    #[serde(rename = "startupSpacingMs", default)]
    pub startup_spacing_ms: u64,
    #[serde(rename = "maxConcurrent")]
    pub max_concurrent: usize,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub launch: LaunchDefinition,
    pub observation: ObservationDefinition,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchDefinition {
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationDefinition {
    pub source: String,
    pub policy: String,
}

#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub profile: AgentProfile,
    pub tool: Option<String>,
    /// Signed provider identity requested for this agent, when it is declared.
    pub provider: Option<String>,
    /// Absolute provider executable pinned for the managed invocation envelope, when declared.
    pub provider_bin: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub expected_model: Option<String>,
    pub expected_effort: Option<String>,
    pub roles: Vec<String>,
    pub responsibility: String,
    pub injectable: bool,
    pub mode: Option<String>,
    pub session_id: String,
    pub poke_hint: String,
    pub legacy_wake: Option<WakeSpec>,
    pub tool_definition: Option<ToolDefinition>,
    pub observation: Option<ObservationDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedInvocationBinding {
    /// Provider pin that must reach a provider-aware legacy wrapper unchanged.
    pub requested_provider: Option<String>,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
    pub observation_source: Option<String>,
    pub observation_policy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedInvocation
{
    pub tool: Option<String>,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Provider pin carried with the rendered invocation for durable auditing.
    pub requested_provider: Option<String>,
    pub requested_model: Option<String>,
    pub requested_effort: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(rename = "apiVersion", default)]
    api_version: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    agents: BTreeMap<String, RawAgentDefinition>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgentDefinition {
    injectable: bool,
    #[serde(default)]
    mode: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: String,
    #[serde(default)]
    wake: Option<RawWakeTemplate>,
    #[serde(rename = "pokeHint", default)]
    poke_hint: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(rename = "providerBin", default)]
    provider_bin: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(rename = "expectedModel", default)]
    expected_model: Option<String>,
    #[serde(rename = "expectedEffort", default)]
    expected_effort: Option<String>,
    #[serde(default)]
    observation: Option<ObservationDefinition>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    responsibility: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWakeTemplate {
    argv: Vec<String>,
    #[serde(rename = "freshArgv", default)]
    _fresh_argv: Vec<String>,
}

fn read_tool(root: &Path, name: &str) -> Result<ToolDefinition> {
    let path = root.join(format!("coordination/tools/{name}.yaml"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 ToolDefinition 失败: {}", path.display()))?;
    let tool: ToolDefinition = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 ToolDefinition 失败: {}", path.display()))?;
    validate_tool(&tool, name)?;
    Ok(tool)
}

fn validate_tool(tool: &ToolDefinition, file_name: &str) -> Result<()> {
    if tool.api_version != "orch/v1alpha1" || tool.kind != "ToolDefinition" {
        bail!("工具 {file_name} 的 apiVersion/kind 不匹配");
    }
    if tool.tool.trim().is_empty() || tool.tool != file_name {
        bail!("工具键与 ToolDefinition.tool 不一致: {file_name}");
    }
    if tool.max_concurrent == 0 {
        bail!("工具 {file_name} maxConcurrent 为 0");
    }
    if tool.launch.argv.is_empty() || tool.launch.argv[0].trim().is_empty() {
        bail!("工具 {file_name} launch.argv 不能为空");
    }
    for key in tool.env.keys() {
        if !matches!(key.as_str(), "model" | "effort") || tool.env[key].trim().is_empty() {
            bail!("工具 {file_name} env 只允许非空 model/effort 映射");
        }
    }
    if tool.model_source == ModelSource::ExternalConfig
        && tool
            .launch
            .argv
            .iter()
            .any(|arg| arg.contains("{model}") || arg.contains("{effort}"))
    {
        bail!("external-config 工具 {file_name} 不得含 model/effort 注入占位");
    }
    if tool.model_source != ModelSource::Env && !tool.env.is_empty() {
        bail!("工具 {file_name} 只有 modelSource=env 才能声明 env 映射");
    }
    if tool.model_source == ModelSource::Env
        && tool
            .launch
            .argv
            .iter()
            .any(|arg| arg.contains("{model}") || arg.contains("{effort}"))
    {
        bail!("env 工具 {file_name} 不得把 model/effort 放进 argv");
    }
    validate_observation(&tool.observation, &format!("工具 {file_name}"))?;
    Ok(())
}

fn validate_observation(observation: &ObservationDefinition, owner: &str) -> Result<()> {
    if observation.source.trim().is_empty() || observation.policy.trim().is_empty() {
        bail!("{owner} observation.source/policy 不能为空或全空白");
    }
    Ok(())
}

fn nonempty(value: Option<String>, field: &str, agent: &str) -> Result<Option<String>> {
    if value
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        bail!("agent {agent} {field} 不能为空或全空白");
    }
    Ok(value)
}

/// Load and validate the complete registry. Any malformed tool or agent
/// rejects the whole table; callers must not silently fall back to zero agents.
pub fn load_agent_definitions(root: &Path) -> Result<BTreeMap<String, AgentDefinition>> {
    let path = root.join("coordination/agents.yaml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取 AgentRegistry 失败: {}", path.display()))?;
    let registry: RegistryFile = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 AgentRegistry 失败: {}", path.display()))?;
    if registry
        .api_version
        .as_deref()
        .is_some_and(|value| value != "orch/v1alpha1")
        || registry
            .kind
            .as_deref()
            .is_some_and(|value| value != "AgentRegistry")
    {
        bail!("AgentRegistry 的 apiVersion/kind 不匹配");
    }

    let mut tools = HashMap::new();
    for raw in registry.agents.values() {
        if let Some(name) = raw.tool.as_deref() {
            if !tools.contains_key(name) {
                tools.insert(name.to_string(), read_tool(root, name)?);
            }
        }
    }

    let mut definitions = BTreeMap::new();
    let mut profiles = Vec::new();
    let mut triples = HashSet::new();
    for (id, raw) in registry.agents {
        if id.trim().is_empty() {
            bail!("AgentRegistry 含空 agent id");
        }
        let provider = nonempty(raw.provider, "provider", &id)?;
        let provider_bin = nonempty(raw.provider_bin, "providerBin", &id)?;
        if provider_bin
            .as_deref()
            .is_some_and(|value| !Path::new(value).is_absolute())
        {
            bail!("agent {id} providerBin 必须是绝对路径");
        }
        let model = nonempty(raw.model, "model", &id)?;
        let effort = nonempty(raw.effort, "effort", &id)?;
        let expected_model = nonempty(raw.expected_model, "expectedModel", &id)?;
        let expected_effort = nonempty(raw.expected_effort, "expectedEffort", &id)?;
        let observation = raw.observation.clone();
        let (profile_capacity, tool_definition) = if let Some(tool_name) = raw.tool.as_deref() {
            if raw.wake.is_some() {
                bail!("agent {id} 同时声明 tool 与 wake，语义互斥");
            }
            if observation.is_some() {
                bail!("tool-backed agent {id} 的 observation 必须由 ToolDefinition 声明");
            }
            let tool = tools
                .get(tool_name)
                .with_context(|| format!("agent {id} 引用未加载工具 {tool_name}"))?;
            if tool.model_source == ModelSource::ExternalConfig {
                if model.is_some() || effort.is_some() {
                    bail!("external-config agent {id} 不得声明 model/effort");
                }
            } else {
                if tool.launch.argv.iter().any(|arg| arg.contains("{model}")) && model.is_none() {
                    bail!("agent {id} 缺少模板所需 model");
                }
                if tool.launch.argv.iter().any(|arg| arg.contains("{effort}")) && effort.is_none() {
                    bail!("agent {id} 缺少模板所需 effort");
                }
            }
            if tool.model_source == ModelSource::Env {
                if model.is_some() && !tool.env.contains_key("model") {
                    bail!("env agent {id} 声明 model 但工具未声明 env.model");
                }
                if effort.is_some() && !tool.env.contains_key("effort") {
                    bail!("env agent {id} 声明 effort 但工具未声明 env.effort");
                }
            }
            if let Some(model) = model.as_deref() {
                if let Some(effort) = effort.as_deref() {
                    let triple = (tool_name.to_string(), model.to_string(), effort.to_string());
                    if !triples.insert(triple) {
                        bail!("重复 tool/model/effort 三元组: {id}");
                    }
                }
            }
            (tool.max_concurrent, Some(tool.clone()))
        } else {
            let wake = raw
                .wake
                .as_ref()
                .with_context(|| format!("legacy agent {id} 缺少 wake"))?;
            if raw.injectable && wake.argv.is_empty() {
                bail!("legacy agent {id} wake.argv 不能为空");
            }
            if expected_model.is_some() || expected_effort.is_some() {
                bail!("legacy agent {id} 不得声明 expectedModel/expectedEffort 字段");
            }
            if (provider.is_some() || model.is_some() || effort.is_some()) && observation.is_none()
            {
                bail!(
                    "legacy agent {id} 声明 provider/model/effort 时必须同时声明 observation.source/policy"
                );
            }
            if let Some(observation) = observation.as_ref() {
                validate_observation(observation, &format!("legacy agent {id}"))?;
            }
            (1, None)
        };

        let legacy_wake = raw.wake.map(|wake| WakeSpec {
            injectable: raw.injectable,
            session_id: raw.session_id.clone(),
            argv: wake.argv,
        });
        let quota_domain = raw.tool.clone().unwrap_or_else(|| id.clone());
        let profile = AgentProfile {
            id: id.clone(),
            capabilities: raw.roles.clone(),
            quota_domain,
            max_concurrent: profile_capacity,
            quality_class: QualityClass::Standard,
        };
        profiles.push(profile.clone());
        definitions.insert(
            id,
            AgentDefinition {
                profile,
                tool: raw.tool,
                provider,
                provider_bin,
                model,
                effort,
                expected_model,
                expected_effort,
                roles: raw.roles,
                responsibility: raw.responsibility,
                injectable: raw.injectable,
                mode: raw.mode,
                session_id: raw.session_id,
                poke_hint: raw.poke_hint,
                legacy_wake,
                tool_definition,
                observation,
            },
        );
    }
    validate_profiles(&profiles).map_err(|errors| anyhow::anyhow!(errors.join("; ")))?;
    Ok(definitions)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn fold_agent_pin_amendments(
    genesis: &str,
    amendments: &[AgentPinAmendment],
) -> Result<String> {
    if !valid_digest(genesis) {
        bail!("registry amendment genesis digest 必须为 64 位小写 hex");
    }
    let mut expected = genesis.to_string();
    let mut latest_pin = BTreeMap::<String, AgentPin>::new();
    for amendment in amendments {
        if amendment.agent.trim().is_empty()
            || amendment.reason.trim().is_empty()
            || amendment.before == amendment.after
            || !valid_digest(&amendment.registry_digest_before)
            || !valid_digest(&amendment.registry_digest_after)
            || amendment.registry_digest_before == amendment.registry_digest_after
        {
            bail!("AgentPinAmended record 非 canonical 或为空转");
        }
        if amendment.registry_digest_before != expected {
            bail!(
                "AgentPinAmended digest 链断裂/乱序: expected={} actual={}",
                expected,
                amendment.registry_digest_before
            );
        }
        if let Some(previous) = latest_pin.get(&amendment.agent) {
            if previous != &amendment.before {
                bail!("AgentPinAmended before pin 与同 agent 上一条 after 不连续");
            }
        }
        latest_pin.insert(amendment.agent.clone(), amendment.after.clone());
        expected = amendment.registry_digest_after.clone();
    }
    Ok(expected)
}

pub(crate) fn decode_agent_pin_amendment_event(
    event: &EventRecord,
    round: &str,
) -> Result<AgentPinAmendment> {
    if event.kind != AGENT_PIN_AMENDED_EVENT_KIND
        || event.actor != "runtime:orch"
        || event.task_id.is_some()
        || event.round.as_deref() != Some(round)
    {
        bail!("AgentPinAmended {} envelope 非 canonical", event.event_id);
    }
    let amendment: AgentPinAmendment = serde_json::from_value(
        event
            .payload
            .clone()
            .with_context(|| format!("AgentPinAmended {} 缺 payload", event.event_id))?,
    )
    .with_context(|| format!("AgentPinAmended {} payload 非 canonical", event.event_id))?;
    if amendment.round != round
        || amendment.ir_revision == 0
        || amendment.actor != "runtime:orch"
        || amendment.reason.trim().is_empty()
    {
        bail!(
            "AgentPinAmended {} attribution 非 canonical",
            event.event_id
        );
    }
    fold_agent_pin_amendments(
        &amendment.registry_digest_before,
        std::slice::from_ref(&amendment),
    )?;
    Ok(amendment)
}

pub fn signed_invocation_binding(def: &AgentDefinition) -> SignedInvocationBinding {
    let observation = def
        .tool_definition
        .as_ref()
        .map(|tool| &tool.observation)
        .or(def.observation.as_ref());
    let requested_by_orch = def
        .tool_definition
        .as_ref()
        .map(|tool| tool.model_source != ModelSource::ExternalConfig)
        .unwrap_or(true);

    SignedInvocationBinding {
        requested_provider: requested_by_orch.then(|| def.provider.clone()).flatten(),
        requested_model: requested_by_orch.then(|| def.model.clone()).flatten(),
        requested_effort: requested_by_orch.then(|| def.effort.clone()).flatten(),
        observation_source: observation.map(|value| value.source.clone()),
        observation_policy: observation.map(|value| value.policy.clone()),
    }
}

fn legacy_pin_env(argv: &[String], binding: &SignedInvocationBinding) -> BTreeMap<String, String> {
    let Some(prefix) = (match argv.get(1).map(String::as_str) {
        Some("orch/scripts/wake-pi-stream.sh") => Some("ORCH_PI"),
        Some("orch/scripts/wake-zcode-stream.sh") => Some("ORCH_ZCODE"),
        Some("orch/scripts/wake-dsh-stream.sh") => Some("ORCH_DSH"),
        _ => None,
    }) else {
        return BTreeMap::new();
    };

    let mut env = BTreeMap::new();
    if let Some(provider) = binding.requested_provider.as_ref() {
        env.insert(format!("{prefix}_PROVIDER"), provider.clone());
    }
    if let Some(model) = binding.requested_model.as_ref() {
        env.insert(format!("{prefix}_MODEL"), model.clone());
    }
    if let Some(effort) = binding.requested_effort.as_ref() {
        env.insert(format!("{prefix}_EFFORT"), effort.clone());
    }
    env
}

/// Render a legacy wake after its session mode has selected the final argv.
///
/// The function preserves the declared argv verbatim while forwarding only
/// signed provider/model/effort pins to the two managed legacy wrappers. A
/// missing declaration stays missing so the wrapper can reject before a model
/// process is started instead of inheriting a local default.
pub fn render_legacy_invocation(def: &AgentDefinition, argv: Vec<String>) -> RenderedInvocation
{
    let binding = signed_invocation_binding(def);
    let env = legacy_pin_env(&argv, &binding);
    RenderedInvocation {
        tool: None,
        argv,
        env,
        requested_provider: binding.requested_provider,
        requested_model: binding.requested_model,
        requested_effort: binding.requested_effort,
    }
}

fn render_template(
    template: &[String],
    session: &str,
    message: &str,
    model: Option<&str>,
    effort: Option<&str>,
    root: &Path,
) -> Result<Vec<String>, String> {
    if template.is_empty() {
        return Err("launch.argv 不能为空".to_string());
    }
    if session.is_empty() {
        if template.iter().any(|arg| arg.contains("{session}")) {
            return Err("sessionId 尚未回填，无法渲染注册表调用".to_string());
        }
    }
    if template.iter().any(|arg| arg.contains("{model}")) && model.is_none() {
        return Err("模板要求 model，但 agent 未声明 model".to_string());
    }
    if template.iter().any(|arg| arg.contains("{effort}")) && effort.is_none() {
        return Err("模板要求 effort，但 agent 未声明 effort".to_string());
    }
    let argv = template
        .iter()
        .map(|arg| {
            arg.replace("{session}", session)
                .replace("{message}", message)
                .replace("{model}", model.unwrap_or(""))
                .replace("{effort}", effort.unwrap_or(""))
                .replace("{root}", &root.display().to_string())
        })
        .collect::<Vec<_>>();
    if argv
        .iter()
        .any(|arg| arg.contains('{') || arg.contains('}'))
    {
        return Err(format!("渲染后残留未解析占位: {argv:?}"));
    }
    if argv[0] != template[0] {
        return Err("argv[0] 不得被注册表渲染改写".to_string());
    }
    Ok(argv)
}

pub fn render_invocation(
    def: &AgentDefinition,
    root: &Path,
    session: &str,
    message: &str,
) -> Result<RenderedInvocation> {
    if let Some(wake) = def.legacy_wake.as_ref() {
        return Ok(render_legacy_invocation(
            def,
            render_wake_argv(wake, message).map_err(anyhow::Error::msg)?,
        ));
    }
    let binding = signed_invocation_binding(def);
    let tool = def
        .tool_definition
        .as_ref()
        .context("modern agent 缺少 ToolDefinition")?;
    let argv = render_template(
        &tool.launch.argv,
        session,
        message,
        def.model.as_deref(),
        def.effort.as_deref(),
        root,
    )
    .map_err(anyhow::Error::msg)?;
    let mut env = BTreeMap::new();
    env.insert("ORCH_AGENT_NAME".to_string(), def.profile.id.clone());
    if tool.model_source == ModelSource::Env {
        if let (Some(name), Some(value)) = (tool.env.get("model"), def.model.as_ref()) {
            env.insert("ORCH_AGENT_MODEL".to_string(), value.clone());
            env.insert(name.clone(), value.clone());
        }
        if let (Some(name), Some(value)) = (tool.env.get("effort"), def.effort.as_ref()) {
            env.insert("ORCH_AGENT_EFFORT".to_string(), value.clone());
            env.insert(name.clone(), value.clone());
        }
    }
    Ok(RenderedInvocation {
        tool: def.tool.clone(),
        argv,
        env,
        requested_provider: binding.requested_provider,
        requested_model: binding.requested_model,
        requested_effort: binding.requested_effort,
    })
}

pub fn plan_startup_wait(
    def: &AgentDefinition,
    last_launch: Option<SystemTime>,
    now: SystemTime,
) -> Result<Duration> {
    let Some(tool) = def.tool_definition.as_ref() else {
        return Ok(Duration::ZERO);
    };
    let spacing = Duration::from_millis(tool.startup_spacing_ms);
    let Some(last) = last_launch else {
        return Ok(Duration::ZERO);
    };
    let elapsed = now
        .duration_since(last)
        .map_err(|_| anyhow::anyhow!("启动时间倒退，拒绝计算启动闸"))?;
    Ok(spacing.saturating_sub(elapsed))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use super::*;
        use std::path::PathBuf;

    fn test_root(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("target/test-tmp")
            .join(format!("b233-registry-{name}-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove stale test root");
        }
        fs::create_dir_all(root.join("coordination/tools")).expect("create test root");
        root
    }

    #[test]
    fn modern_agent_rejects_simultaneous_tool_and_legacy_wake() {
        let root = test_root("dual-shape");
        fs::write(
            root.join("coordination/tools/agy.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: agy
modelSource: argv
startupSpacingMs: 0
maxConcurrent: 1
launch:
  argv: ["agy", "--model", "{model}", "--effort", "{effort}", "{message}"]
observation:
  source: stdout
  policy: strict
"#,
        )
        .expect("write tool definition");
        fs::write(
            root.join("coordination/agents.yaml"),
            r#"agents:
  executor-dual:
    injectable: true
    sessionId: dual-session
    tool: agy
    model: test-model
    effort: high
    wake:
      argv: ["agy", "{message}"]
"#,
        )
        .expect("write agent registry");

        let error = load_agent_definitions(&root).expect_err("dual-shape agent must fail loud");
        assert!(
            error.to_string().contains("同时声明 tool 与 wake"),
            "unexpected error: {error:#}"
        );

        fs::remove_dir_all(root).expect("remove test root");
    }
}

#[cfg(test)]
mod historical_amendment_reader_tests {
    use super::*;
fn amendment(before_digest: &str, after_digest: &str, before_model: &str, after_model: &str) -> AgentPinAmendment {
    serde_json::from_value(serde_json::json!({
        "agent":"fixture-agent",
        "before":{"provider":"fixture-provider","model":before_model,"effort":"max"},
        "after":{"provider":"fixture-provider","model":after_model,"effort":"max"},
        "registryDigestBefore":before_digest,"registryDigestAfter":after_digest,
        "round":"r9999","irRevision":1,"actor":"runtime:orch","reason":"historical reader fixture"
    })).unwrap()
}
#[test]
fn amendments_fold_in_ledger_order() {
    let a="a".repeat(64);let b="b".repeat(64);let c="c".repeat(64);
    let first=amendment(&a,&b,"old","middle");let second=amendment(&b,&c,"middle","new");
    assert_eq!(fold_agent_pin_amendments(&a,&[first.clone(),second.clone()]).unwrap(),c);
    assert!(fold_agent_pin_amendments(&a,&[second,first]).is_err());
    assert!(fold_agent_pin_amendments(&a,&[amendment(&a,&b,"old","middle"),amendment(&b,&c,"wrong","new")]).is_err());
    assert!(fold_agent_pin_amendments(&a,&[amendment(&a,&a,"old","new")]).is_err());
    assert!(fold_agent_pin_amendments(&a,&[amendment(&a,&b,"same","same")]).is_err());
}

}
