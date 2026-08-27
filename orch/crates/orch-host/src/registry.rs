//! Agent/tool registry v1 (B233).
//!
//! The legacy wake schema remains owned by `wake.rs`.  This module adds the
//! typed tool/model layer around it without changing the frozen `WakeSpec` or
//! `AgentProfile` shapes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};
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

/// Replayable description of one surgical provider/model/effort amendment.
///
/// Production records carry a real round, revision, and runtime actor.  The
/// lower-level [`amend_agent_pin`] primitive deliberately returns an unscoped
/// record so fixture callers can prove the byte edit and digest transition;
/// [`run_agent_pin_amendment`] is the only production entry point that scopes
/// the record and appends it to the ledger.
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

fn pin_of(definition: &AgentDefinition) -> AgentPin {
    AgentPin {
        provider: definition.provider.clone(),
        model: definition.model.clone(),
        effort: definition.effort.clone(),
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_pin_scalar(value: Option<&str>, field: &str) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() || value != value.trim() {
        bail!("agent pin {field} 不能为空、全空白或带首尾空白");
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':')
    }) {
        bail!("agent pin {field} 含不能安全外科写入 YAML plain scalar 的字符: {value:?}");
    }
    Ok(())
}

fn invocation_argv(definition: &AgentDefinition) -> Option<&[String]> {
    definition
        .legacy_wake
        .as_ref()
        .map(|wake| wake.argv.as_slice())
        .or_else(|| {
            definition
                .tool_definition
                .as_ref()
                .map(|tool| tool.launch.argv.as_slice())
        })
}

fn argv_has_literal_after(argv: &[String], flags: &[&str], placeholder: &str) -> bool {
    argv.iter().enumerate().any(|(index, argument)| {
        if flags.contains(&argument.as_str()) {
            return argv
                .get(index + 1)
                .is_some_and(|value| !value.contains(placeholder));
        }
        flags.iter().any(|flag| {
            argument
                .strip_prefix(&format!("{flag}="))
                .is_some_and(|value| !value.contains(placeholder))
        })
    })
}

fn reject_inline_pin_literal(
    definition: &AgentDefinition,
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<()> {
    let Some(argv) = invocation_argv(definition) else {
        return Ok(());
    };
    let contains_current = |current: Option<&str>| {
        current.is_some_and(|value| {
            argv.iter()
                .any(|argument| argument == value || argument.contains(&format!("={value}")))
        })
    };
    let model_literal = model.is_some()
        && (argv_has_literal_after(argv, &["-m", "--model"], "{model}")
            || contains_current(definition.model.as_deref()));
    let effort_literal = effort.is_some()
        && (argv_has_literal_after(
            argv,
            &["--effort", "--variant", "--thought-level"],
            "{effort}",
        ) || contains_current(definition.effort.as_deref())
            || argv.iter().any(|argument| {
                argument.contains("effort=") || argument.contains("reasoning_effort=")
            }));
    let provider_literal = provider.is_some()
        && (argv_has_literal_after(argv, &["--provider"], "{provider}")
            || contains_current(definition.provider.as_deref()));
    if model_literal || effort_literal || provider_literal {
        bail!(
            "agent {} 的 invocation argv 内联了待修订 pin；agent set-pin 不同步 argv，必须走完整 orch plan + 重签",
            definition.profile.id
        );
    }
    Ok(())
}

fn tool_definition_equal(left: &ToolDefinition, right: &ToolDefinition) -> bool {
    left.api_version == right.api_version
        && left.kind == right.kind
        && left.tool == right.tool
        && left.model_source == right.model_source
        && left.effort_semantic == right.effort_semantic
        && left.startup_spacing_ms == right.startup_spacing_ms
        && left.max_concurrent == right.max_concurrent
        && left.env == right.env
        && left.launch.argv == right.launch.argv
        && left.observation == right.observation
}

fn optional_tool_definition_equal(
    left: Option<&ToolDefinition>,
    right: Option<&ToolDefinition>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => tool_definition_equal(left, right),
        _ => false,
    }
}

fn non_pin_definition_equal(left: &AgentDefinition, right: &AgentDefinition) -> bool {
    left.profile == right.profile
        && left.tool == right.tool
        && left.expected_model == right.expected_model
        && left.expected_effort == right.expected_effort
        && left.roles == right.roles
        && left.responsibility == right.responsibility
        && left.injectable == right.injectable
        && left.mode == right.mode
        && left.session_id == right.session_id
        && left.poke_hint == right.poke_hint
        && left.legacy_wake == right.legacy_wake
        && optional_tool_definition_equal(
            left.tool_definition.as_ref(),
            right.tool_definition.as_ref(),
        )
        && left.observation == right.observation
}

fn validate_only_requested_pins_changed(
    before: &BTreeMap<String, AgentDefinition>,
    after: &BTreeMap<String, AgentDefinition>,
    agent: &str,
    expected: &AgentPin,
) -> Result<()> {
    if before.len() != after.len() || before.keys().ne(after.keys()) {
        bail!("agent set-pin 不得增删或重命名 registry agent");
    }
    for (id, before_definition) in before {
        let after_definition = &after[id];
        if !non_pin_definition_equal(before_definition, after_definition) {
            bail!("agent set-pin 检出第四字段差异: agent={id}");
        }
        let after_pin = pin_of(after_definition);
        if id == agent {
            if &after_pin != expected {
                bail!("agent set-pin 写后 pin 与请求不一致: agent={agent}");
            }
        } else if after_pin != pin_of(before_definition) {
            bail!("agent set-pin 不得改动未指名 agent 的 pin: agent={id}");
        }
    }
    Ok(())
}

fn line_body(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

fn direct_agent_field(line: &str, field: &str) -> bool {
    let (body, _) = line_body(line);
    body.starts_with("    ")
        && !body.starts_with("      ")
        && body
            .trim_start()
            .strip_prefix(field)
            .is_some_and(|rest| rest.starts_with(':'))
}

fn agent_block(lines: &[String], agent: &str) -> Result<(usize, usize)> {
    let header = format!("  {agent}:");
    let start = lines
        .iter()
        .position(|line| line_body(line).0 == header)
        .with_context(|| format!("agents.yaml 未找到可外科编辑的 agent block: {agent}"))?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(index, line)| {
            let body = line_body(line).0;
            (body.starts_with("  ")
                && !body.starts_with("    ")
                && !body.trim_start().starts_with('#'))
            .then_some(index)
        })
        .unwrap_or(lines.len());
    Ok((start, end))
}

fn replace_scalar_line(line: &str, field: &str, value: &str) -> Result<String> {
    let (body, newline) = line_body(line);
    let prefix = format!("    {field}:");
    let rest = body
        .strip_prefix(&prefix)
        .with_context(|| format!("agents.yaml {field} 行缩进/形状异常"))?;
    let suffix = rest
        .find(" #")
        .map(|position| &rest[position..])
        .unwrap_or("");
    Ok(format!("{prefix} {value}{suffix}{newline}"))
}

fn surgical_registry_text(
    source: &str,
    agent: &str,
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<String> {
    let mut lines = source
        .split_inclusive('\n')
        .map(String::from)
        .collect::<Vec<_>>();
    if source.is_empty() {
        bail!("agents.yaml 不能为空");
    }
    for (field, value) in [("provider", provider), ("model", model), ("effort", effort)] {
        let Some(value) = value else {
            continue;
        };
        let (start, end) = agent_block(&lines, agent)?;
        if let Some(index) = lines[start + 1..end]
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, line)| direct_agent_field(line, field).then_some(start + 1 + index))
        {
            lines[index] = replace_scalar_line(&lines[index], field, value)?;
        } else {
            let newline = lines
                .get(end.saturating_sub(1))
                .map(|line| line_body(line).1)
                .filter(|newline| !newline.is_empty())
                .unwrap_or("\n");
            lines.insert(end, format!("    {field}: {value}{newline}"));
        }
    }
    Ok(lines.concat())
}

fn atomic_replace_registry(path: &Path, bytes: &[u8]) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("检查 AgentRegistry target 失败: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("AgentRegistry target 必须是 regular non-symlink file");
    }
    let parent = path.parent().context("AgentRegistry target 缺 parent")?;
    let temporary = parent.join(format!(
        ".agents.yaml.pin-tmp-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("创建 AgentRegistry 临时文件失败: {}", temporary.display()))?;
        file.write_all(bytes)
            .context("写 AgentRegistry 临时文件失败")?;
        file.sync_all()
            .context("fsync AgentRegistry 临时文件失败")?;
        drop(file);
        fs::rename(&temporary, path).context("原子替换 AgentRegistry 失败")?;
        File::open(parent)
            .context("打开 AgentRegistry parent 以 fsync 失败")?
            .sync_all()
            .context("fsync AgentRegistry parent 失败")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn rollback_registry(path: &Path, original: &[u8], cause: anyhow::Error) -> anyhow::Error {
    match atomic_replace_registry(path, original) {
        Ok(()) => cause.context("agent pin amendment 已完整回滚"),
        Err(rollback) => anyhow::anyhow!(
            "agent pin amendment 失败: {cause:#}; AgentRegistry rollback 也失败: {rollback:#}"
        ),
    }
}

/// Surgically amend one registry pin and return the replayable digest delta.
///
/// This primitive validates the parsed registry before and after the edit and
/// rolls the file back on every modeled error.  It does not append a ledger
/// fact or check round state; production callers must use
/// [`run_agent_pin_amendment`] so the byte change and durable event are one
/// protocol transition.
pub fn amend_agent_pin(
    root: &Path,
    agent: &str,
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reason: &str,
) -> Result<AgentPinAmendment> {
    if agent.trim().is_empty() || agent != agent.trim() {
        bail!("agent id 不能为空或带首尾空白");
    }
    if reason.trim().is_empty() {
        bail!("agent set-pin --reason 不得为空白");
    }
    validate_pin_scalar(provider, "provider")?;
    validate_pin_scalar(model, "model")?;
    validate_pin_scalar(effort, "effort")?;
    if provider.is_none() && model.is_none() && effort.is_none() {
        bail!("agent set-pin 至少指定 provider/model/effort 之一");
    }

    let definitions_before = load_agent_definitions(root)?;
    let definition = definitions_before
        .get(agent)
        .with_context(|| format!("AgentRegistry 未注册 agent: {agent}"))?;
    let before = pin_of(definition);
    let after = AgentPin {
        provider: provider
            .map(String::from)
            .or_else(|| before.provider.clone()),
        model: model.map(String::from).or_else(|| before.model.clone()),
        effort: effort.map(String::from).or_else(|| before.effort.clone()),
    };
    if before == after {
        bail!("agent set-pin 请求与现值完全相同，拒绝空转");
    }
    reject_inline_pin_literal(definition, provider, model, effort)?;

    let path = root.join("coordination/agents.yaml");
    let original = fs::read(&path)
        .with_context(|| format!("读取 AgentRegistry bytes 失败: {}", path.display()))?;
    let original_text = std::str::from_utf8(&original).context("AgentRegistry 必须为 UTF-8")?;
    let candidate = surgical_registry_text(original_text, agent, provider, model, effort)?;
    let digest_before = crate::plan::agent_registry_digest(root)?;

    if let Err(error) = atomic_replace_registry(&path, candidate.as_bytes()) {
        let current = fs::read(&path).unwrap_or_default();
        return if current == original {
            Err(error)
        } else {
            Err(rollback_registry(&path, &original, error))
        };
    }
    let validated = (|| -> Result<AgentPinAmendment> {
        let definitions_after = load_agent_definitions(root)?;
        validate_only_requested_pins_changed(
            &definitions_before,
            &definitions_after,
            agent,
            &after,
        )?;
        let digest_after = crate::plan::agent_registry_digest(root)?;
        if digest_before == digest_after {
            bail!("agent set-pin 改写后 registry digest 未变化");
        }
        Ok(AgentPinAmendment {
            agent: agent.to_string(),
            before,
            after,
            registry_digest_before: digest_before,
            registry_digest_after: digest_after,
            round: "unscoped".to_string(),
            ir_revision: 0,
            actor: "runtime:orch".to_string(),
            reason: reason.trim().to_string(),
        })
    })();
    validated.map_err(|error| rollback_registry(&path, &original, error))
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

fn append_amendment_event(
    root: &Path,
    round: &str,
    initial_event_ids: &[String],
    event: &EventRecord,
) -> Result<()> {
    let event_id = event.event_id.clone();
    let candidate = event.clone();
    let appended = crate::ledger::append_checked(root, round, |fresh| {
        if fresh.iter().any(|item| item.event_id == event_id) {
            return Ok(Vec::new());
        }
        if fresh.len() != initial_event_ids.len()
            || fresh
                .iter()
                .zip(initial_event_ids)
                .any(|(item, expected)| &item.event_id != expected)
        {
            bail!("AgentPinAmended 事务内账本快照漂移");
        }
        Ok(vec![candidate])
    })?;
    if appended > 1 {
        bail!("AgentPinAmended append count 非法: {appended}");
    }
    Ok(())
}

/// Amend an agent pin inside an open, user-signed round and durably record it.
///
/// The protocol transition serializes the round check, surgical file change,
/// and WAL-backed ledger append.  A failed append is retried once for atomic
/// ledger recovery/idempotency; if no exact event committed, the original
/// registry bytes are restored before the error is returned.
pub fn run_agent_pin_amendment(
    root: &Path,
    agent: &str,
    provider: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    reason: &str,
) -> Result<AgentPinAmendment> {
    crate::close::with_protocol_transition(root, "orch agent set-pin", || {
        let round = crate::current_round(root)?;
        let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
        let ledger = read_ledger(&ledger_path)
            .with_context(|| format!("读取 AgentPinAmended 账本失败: {}", ledger_path.display()))?;
        if !ledger.bad_lines.is_empty() {
            bail!("agent set-pin 拒绝坏账本");
        }
        if orch_core::fold(&ledger.events).round_closed {
            bail!("轮 {round} 已收轮，拒绝 agent set-pin");
        }
        let active = crate::plan::require_active_round_ir(root, &round, &ledger.events)?;
        let initial_event_ids = ledger
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect::<Vec<_>>();
        let registry_path = root.join("coordination/agents.yaml");
        let original = fs::read(&registry_path).with_context(|| {
            format!(
                "读取 agent set-pin rollback bytes 失败: {}",
                registry_path.display()
            )
        })?;

        let mut amendment = amend_agent_pin(root, agent, provider, model, effort, reason)?;
        amendment.round = round.clone();
        amendment.ir_revision = active.persisted_revision;
        amendment.actor = "runtime:orch".to_string();
        let event = crate::ledger::event(
            AGENT_PIN_AMENDED_EVENT_KIND,
            "runtime:orch",
            None,
            Some(&round),
            serde_json::to_value(&amendment)?,
        );

        let first = append_amendment_event(root, &round, &initial_event_ids, &event);
        if let Err(first_error) = first {
            if let Err(second_error) =
                append_amendment_event(root, &round, &initial_event_ids, &event)
            {
                let fresh = read_ledger(&ledger_path).ok();
                let committed = fresh.as_ref().is_some_and(|ledger| {
                    ledger.bad_lines.is_empty()
                        && ledger
                            .events
                            .iter()
                            .any(|item| item.event_id == event.event_id)
                });
                if !committed {
                    let cause = anyhow::anyhow!(
                        "AgentPinAmended append 两次失败: first={first_error:#}; second={second_error:#}"
                    );
                    return Err(rollback_registry(&registry_path, &original, cause));
                }
            }
        }

        let committed = read_ledger(&ledger_path)
            .with_context(|| format!("回读 AgentPinAmended 账本失败: {}", ledger_path.display()))?;
        if !committed.bad_lines.is_empty()
            || !committed
                .events
                .iter()
                .any(|item| item.event_id == event.event_id)
        {
            let cause = anyhow::anyhow!("AgentPinAmended 未出现在提交后账本");
            return Err(rollback_registry(&registry_path, &original, cause));
        }
        crate::plan::require_active_round_ir(root, &round, &committed.events)
            .context("AgentPinAmended 后 active ROUND-IR 校验失败")?;
        Ok(amendment)
    })
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
    use super::*;
    use std::fs;
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
