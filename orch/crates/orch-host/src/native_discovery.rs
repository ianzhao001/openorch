//! Bounded, non-inference discovery of native client configuration and catalogs.
//! Only model-selection fields leave the parser. Historical sessions are never
//! treated as a catalog, and unavailable native defaults remain unknown.
#![deny(missing_docs)]
use crate::{
    channel::InvocationTuple,
    harness::{DriverAction, HarnessId},
    harness_config::{HarnessAction, HarnessConfigSnapshot},
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};
use wait_timeout::ChildExt;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODELS: usize = 2048;
const CONFIG_OVERRIDES: &[&str] = &[
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "PI_CODING_AGENT_DIR",
    "XDG_CONFIG_HOME",
    "DSH_HOME",
    "ORCH_ZCODE_CONFIG",
    "ORCH_DSH_PROFILE",
    "CLAUDE_CODE_EFFORT_LEVEL",
    "ANTHROPIC_MODEL",
];
#[cfg(target_os = "linux")]
const NO_FOLLOW: i32 = 0x20000;
#[cfg(not(target_os = "linux"))]
const NO_FOLLOW: i32 = 0x100;

/// A native model identifier, not an inferred account entitlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeModel {
    /// Native provider ID when the catalog exposes one.
    pub provider: Option<String>,
    /// Exact native model ID; never replaced by a version-specific alias.
    pub id: String,
    /// Native display label, or the ID when no label is provided.
    pub name: String,
    /// Exact native effort/variant values, possibly an empty unknown list.
    pub efforts: Vec<String>,
    /// Explicit native default for this model, when provided.
    pub default_effort: Option<String>,
}
/// Nonsecret native routing data, captured with the same configuration read as models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeProviderRoute {
    /// Native provider identity; for Claude this is identified from its endpoint.
    pub provider: String,
    /// Endpoint without credentials, query or fragment.
    pub url: String,
    /// Optional environment variable used by the native provider for credentials.
    pub credential_env: Option<String>,
}
fn safe_provider_url(value: &str) -> Option<String> {
    let value=value.trim().trim_end_matches('/');
    ((value.starts_with("https://") || value.starts_with("http://")) && value.len()<=2048
        && !value.contains(['@','?','#']) && !value.chars().any(char::is_control)).then(||value.to_string())
}
fn claude_provider_route(value: &Value) -> Option<NativeProviderRoute> {
    let env=value.get("env")?;
    if ["CLAUDE_CODE_USE_BEDROCK","CLAUDE_CODE_USE_VERTEX"].iter().any(|key|env.get(key).and_then(Value::as_str).is_some_and(|v|v=="1"||v=="true")) {return None;}
    let url=safe_provider_url(env.get("ANTHROPIC_BASE_URL")?.as_str()?)?;
    let authority=url.split_once("://")?.1.split('/').next()?;
    let provider=match authority {"api.deepseek.com"|"api.deepseek.com:443"=>"deepseek","api.anthropic.com"|"api.anthropic.com:443"=>"anthropic",_=>return None};
    Some(NativeProviderRoute { provider:provider.into(),url,credential_env:None })
}
/// A safe projection of one client's current settings and available catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeConfiguration {
    /// Captured private routing metadata; never exposed by ordinary discovery serialization.
    #[serde(skip)]
    pub provider_route: Option<NativeProviderRoute>,
    /// Configured model-selection values; None means not exposed/configured.
    pub current: InvocationTuple,
    /// Native candidates, not a hardcoded product model list.
    pub models: Vec<NativeModel>,
    /// catalog, current-only, unknown, auth-required or unavailable.
    pub status: String,
    /// Safe diagnostics without raw native configuration or stderr.
    pub diagnostics: Vec<String>,
}
impl Default for NativeConfiguration {
    fn default() -> Self {
        Self {
            provider_route: None,
            current: InvocationTuple::default(),
            models: Vec::new(),
            status: "unknown".into(),
            diagnostics: Vec::new(),
        }
    }
}
/// One captured native file's identity. Its bytes are never serialized.
#[derive(Debug, Clone, Serialize)]
pub struct NativeSource {
    /// native-config, native-cache, native-command, bundled, environment or help-derived origin.
    pub kind: String,
    /// Local path or safe code-owned command description.
    pub path: String,
    /// Hash of the single bounded read.
    pub sha256: String,
}
/// A selectable installed client or an existing project alias.
#[derive(Debug, Clone, Serialize)]
pub struct NativeHarness {
    /// Selected consultation backend, distinct from native model/account qualification.
    pub backend: String,
    /// Stable source identity: configured:alias or installed:driver.
    pub id: String,
    /// Code-owned driver name.
    pub driver: String,
    /// Resolved installed executable, never accepted from an HTTP write body.
    pub executable: Option<PathBuf>,
    /// Capability status; distinct from native catalog/login state.
    pub availability: String,
    /// Explicit enabled flag, independent of action support.
    pub enabled: bool,
    /// Safe capability failure reason.
    pub reason: Option<String>,
    /// Project alias when this row came from the immutable harness snapshot.
    pub alias: Option<String>,
    /// Existing project pins for explicit user reference, not silent native fallback.
    pub configured: InvocationTuple,
    /// Current native projection and its honest catalog status.
    pub native: NativeConfiguration,
    /// File sources used to compute the current projection.
    pub sources: Vec<NativeSource>,
}
/// One action-local scan, retaining the existing alias snapshot for later consumers.
#[derive(Clone, Serialize)]
pub struct DiscoverySnapshot {
    /// Time this scan began; it is not a login or model-call receipt.
    pub observed_at: String,
    /// Independent rows; failure in one client does not remove siblings.
    pub harnesses: Vec<NativeHarness>,
    /// Safe project configuration diagnostics.
    pub diagnostics: Vec<String>,
    #[serde(skip)]
    pub(crate) configured_snapshot: Option<HarnessConfigSnapshot>,
}
/// Explicit environment for deterministic tests and the actual local scanner.
#[derive(Debug, Clone)]
pub struct DiscoveryContext {
    /// Exact invocation project, used for native project overrides.
    pub project: PathBuf,
    /// Native client home; never changed process-globally by this module.
    pub home: PathBuf,
    /// Executable search directories captured once.
    pub search_path: Vec<PathBuf>,
    /// Bound for a single non-inference command, in milliseconds.
    pub query_timeout_ms: u64,
    /// False allows only configuration-file projection.
    pub allow_commands: bool,
    /// Captured non-secret native path/model environment overrides.
    pub overrides: BTreeMap<String, String>,
    /// Whether standard platform application bundles may be discovered.
    pub include_platform_locations: bool,
}
impl DiscoveryContext {
    /// Capture the caller's environment once without changing global variables.
    pub fn system(project: &Path) -> Result<Self> {
        Ok(Self {
            project: fs::canonicalize(project)?,
            home: PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?),
            search_path: std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .collect(),
            query_timeout_ms: 10000,
            allow_commands: true,
            overrides: CONFIG_OVERRIDES
                .iter()
                .filter_map(|key| std::env::var(key).ok().map(|value| ((*key).into(), value)))
                .collect(),
            include_platform_locations: true,
        })
    }
}

/// Conservative, read-only assessment of OpenCode's known local state roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OpenCodeReadiness {
    /// Every known root exists as an owned writable directory.
    Ready,
    /// A stable local filesystem fact proves startup unsafe.
    Unsafe {
        /// Bounded, path-free explanation of the proven local defect.
        reason: String,
    },
    /// Missing state or platform semantics prevent a definitive claim.
    Indeterminate {
        /// Bounded explanation of which readiness fact remains unknown.
        reason: String,
    },
}

/// Inspect OpenCode data/log roots without creating files, opening databases,
/// running the client, or claiming anything about login/provider capacity.
pub fn opencode_local_readiness(context: &DiscoveryContext) -> OpenCodeReadiness {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    let location = |key: &str, fallback: PathBuf| {
        context
            .overrides
            .get(key)
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    context.project.join(path)
                }
            })
            .unwrap_or(fallback)
    };
    let roots = [
        location("XDG_DATA_HOME", context.home.join(".local/share")).join("opencode"),
        location("XDG_STATE_HOME", context.home.join(".local/state")).join("opencode"),
    ];
    let uid = unsafe { geteuid() };
    let mut indeterminate = false;
    for path in &roots {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut ancestor = path.parent();
                while let Some(candidate) = ancestor {
                    match fs::symlink_metadata(candidate) {
                        Ok(metadata) => {
                            if metadata.file_type().is_symlink()
                                || fs::canonicalize(candidate).ok().as_deref() != Some(candidate)
                            {
                                return OpenCodeReadiness::Unsafe {
                                    reason: "OpenCode state ancestor is indirect".into(),
                                };
                            }
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ancestor = candidate.parent();
                        }
                        Err(_) => {
                            return OpenCodeReadiness::Indeterminate {
                                reason: "OpenCode state ancestor cannot be inspected".into(),
                            };
                        }
                    }
                }
                indeterminate = true;
                continue;
            }
            Err(_) => {
                return OpenCodeReadiness::Indeterminate {
                    reason: "OpenCode state root cannot be inspected".into(),
                };
            }
        };
        if metadata.file_type().is_symlink()
            || fs::canonicalize(path).ok().as_deref() != Some(path.as_path())
        {
            return OpenCodeReadiness::Unsafe {
                reason: "OpenCode state root is indirect".into(),
            };
        }
        if !metadata.is_dir() {
            return OpenCodeReadiness::Unsafe {
                reason: "OpenCode state root is not a directory".into(),
            };
        }
        if metadata.uid() != uid {
            return OpenCodeReadiness::Unsafe {
                reason: "OpenCode state root has a different owner".into(),
            };
        }
        if metadata.permissions().mode() & 0o222 == 0 {
            return OpenCodeReadiness::Unsafe {
                reason: "OpenCode state root is definitively non-writable".into(),
            };
        }
    }
    if indeterminate {
        OpenCodeReadiness::Indeterminate {
            reason: "OpenCode state root is absent; writability is unproven".into(),
        }
    } else {
        OpenCodeReadiness::Ready
    }
}
fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty() && s.len() <= 512 && s.trim() == *s && !s.chars().any(char::is_control)
        })
        .map(str::to_owned)
}
fn keys(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_object)
        .map(|o| {
            o.keys()
                .filter(|k| text(Some(&Value::String((*k).clone()))).is_some())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}
fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| text(Some(v))).collect())
        .unwrap_or_default()
}
fn model(
    provider: Option<String>,
    id: String,
    value: &Value,
    efforts: Vec<String>,
    default_effort: Option<String>,
) -> NativeModel {
    NativeModel {
        provider,
        id: id.clone(),
        name: text(value.get("name"))
            .or_else(|| text(value.get("display_name")))
            .unwrap_or(id),
        efforts,
        default_effort,
    }
}
fn set_compound(tuple: &mut InvocationTuple, value: Option<String>, separate: bool) {
    if let Some(value) = value {
        if separate {
            if let Some((provider, id)) = value.split_once('/') {
                if !provider.is_empty() && !id.is_empty() {
                    tuple.provider = Some(provider.into());
                    tuple.model = Some(id.into());
                    return;
                }
            }
        }
        tuple.model = Some(value);
    }
}
#[derive(Default)]
struct ClaudeEffortLayer {
    general: Option<String>,
    per_model: BTreeMap<String, String>,
}
fn claude_layer(value: &Value) -> ClaudeEffortLayer {
    let mut layer = ClaudeEffortLayer {
        general: text(value.get("effortLevel")),
        per_model: BTreeMap::new(),
    };
    if let Some(entries) = value.get("modelSettings").and_then(Value::as_object) {
        for (id, entry) in entries {
            if let Some(effort) = text(entry.get("effortLevel")) {
                layer.per_model.insert(id.clone(), effort);
            }
        }
    }
    layer
}
fn claude_effort(layers: &[ClaudeEffortLayer], model: Option<&str>) -> (Option<String>, bool) {
    for layer in layers.iter().rev() {
        if layer.per_model.is_empty() {
            if layer.general.is_some() {
                return (layer.general.clone(), false);
            }
            continue;
        }
        // Canonical aliases belong to the native client. We can establish an
        // exact value without copying a version-specific alias table only when
        // an exact key matches and all potential alias matches agree, or when
        // all possible per-model/top-level outcomes agree.
        if let Some(value) = model.and_then(|m| layer.per_model.get(m)) {
            if layer.per_model.values().all(|v| v == value) {
                return (Some(value.clone()), false);
            }
        }
        if let Some(value) = &layer.general {
            if layer.per_model.values().all(|v| v == value) {
                return (Some(value.clone()), false);
            }
        }
        return (None, true);
    }
    (None, false)
}
fn finish(mut config: NativeConfiguration) -> Result<NativeConfiguration> {
    crate::fusion_roles::validate_tuple(&config.current)?;
    let mut entries = BTreeMap::new();
    for model in config.models.drain(..) {
        if text(Some(&Value::String(model.id.clone()))).is_some()
            && model
                .provider
                .as_ref()
                .is_none_or(|p| text(Some(&Value::String(p.clone()))).is_some())
        {
            entries.insert((model.provider.clone(), model.id.clone()), model);
        }
    }
    config.models = entries.into_values().collect();
    if config.models.len() > MAX_MODELS {
        config.models.truncate(MAX_MODELS);
        config.diagnostics.push("catalog_clipped".into());
    }
    config.status = if !config.models.is_empty() {
        "catalog"
    } else if config.current != InvocationTuple::default() {
        "current-only"
    } else {
        "unknown"
    }
    .into();
    Ok(config)
}
fn jsonc_text(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let (mut out, mut i, mut quoted, mut escape) = (Vec::new(), 0, false, false);
    while i < bytes.len() {
        let c = bytes[i];
        if quoted {
            out.push(c);
            if escape {
                escape = false
            } else if c == b'\\' {
                escape = true
            } else if c == b'"' {
                quoted = false
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            quoted = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' && bytes[i] != b'\r' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            out.extend_from_slice(b"  ");
            i += 2;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    out.extend_from_slice(b"  ");
                    i += 2;
                    closed = true;
                    break;
                }
                out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if !closed {
                bail!("unclosed_native_json_comment");
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    let (mut quote, mut escaped) = (false, false);
    for i in 0..out.len() {
        let c = out[i];
        if quote {
            if escaped {
                escaped = false
            } else if c == b'\\' {
                escaped = true
            } else if c == b'"' {
                quote = false
            }
        } else if c == b'"' {
            quote = true
        } else if c == b',' {
            let mut j = i + 1;
            while j < out.len() && out[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(out.get(j), Some(b'}') | Some(b']')) {
                out[i] = b' ';
            }
        }
    }
    Ok(String::from_utf8(out)?)
}

fn toml_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('"') {
        serde_json::from_str::<String>(value).ok()
    } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        Some(value[1..value.len() - 1].into())
    } else {
        None
    }
}
fn codex_toml(input: &str) -> Result<NativeConfiguration> {
    let mut tables: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut section = String::new();
    for line in input.lines() {
        let mut quote = None;
        let mut escape = false;
        let mut end = line.len();
        for (i, c) in line.char_indices() {
            if let Some(q) = quote {
                if escape {
                    escape = false;
                } else if c == '\\' && q == '"' {
                    escape = true;
                } else if c == q {
                    quote = None;
                }
            } else if c == '"' || c == '\'' {
                quote = Some(c);
            } else if c == '#' {
                end = i;
                break;
            }
        }
        let line = line[..end].trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().replace('"', "");
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if let Some(value) = toml_scalar(value) {
                tables
                    .entry(section.clone())
                    .or_default()
                    .insert(key.trim().into(), value);
            }
        }
    }
    let mut values = tables.get("").cloned().unwrap_or_default();
    if values.contains_key("profile") {
        bail!("codex_explicit_profile_context_required");
    }
    let provider_route=values.get("model_provider").and_then(|provider| {
        let table=tables.get(&format!("model_providers.{provider}"))?;
        let url=safe_provider_url(table.get("base_url")?)?;
        Some(NativeProviderRoute { provider:provider.clone(),url,credential_env:table.get("env_key").cloned() })
    });
    finish(NativeConfiguration {
        provider_route,
        current: InvocationTuple {
            model: values.remove("model"),
            effort: values.remove("model_reasoning_effort"),
            ..Default::default()
        },
        ..Default::default()
    })
}
/// Project native fields without copying credentials, arbitrary provider options or history.
/// Unknown fields are ignored; unsupported formats produce an honest error/unknown result.
pub fn parse_native_config(driver: &str, input: &str) -> Result<NativeConfiguration> {
    if input.len() > MAX_BYTES {
        bail!("native_config_too_large");
    }
    if driver == "codex" && !input.trim_start().starts_with('{') {
        return codex_toml(input);
    }
    let value: Value = if driver == "dsh" {
        let yaml: serde_yaml::Value = serde_yaml::from_str(input).context("invalid_native_yaml")?;
        serde_json::to_value(yaml)?
    } else {
        serde_json::from_str(&jsonc_text(input)?).context("invalid_native_json")?
    };
    if !value.is_object() {
        bail!("native_config_not_object");
    }
    let mut out = NativeConfiguration::default();
    match driver {
        "zcode" => {
            let selection = value
                .get("model")
                .and_then(|m| m.get("main"))
                .or_else(|| value.get("model"));
            if let Some(m) = selection.filter(|m| m.is_object()) {
                out.current.provider = text(m.get("provider"));
                out.current.model = text(m.get("model"));
            } else {
                set_compound(&mut out.current, text(selection), true);
            }
            if let Some(providers) = value.get("provider").and_then(Value::as_object) {
                for (pid, p) in providers {
                    if let Some(models) = p.get("models").and_then(Value::as_object) {
                        for (id, m) in models {
                            let r = m.get("reasoning").unwrap_or(&Value::Null);
                            out.models.push(model(
                                Some(pid.clone()),
                                id.clone(),
                                m,
                                strings(r.get("levels")),
                                text(r.get("defaultLevel")),
                            ));
                        }
                    }
                }
            }
            if let Some(m) = out.models.iter().find(|m| {
                m.provider == out.current.provider && Some(&m.id) == out.current.model.as_ref()
            }) {
                out.current.effort = m.default_effort.clone();
            }
        }
        "pi" => {
            out.current.provider = text(value.get("defaultProvider"));
            out.current.model = text(value.get("defaultModel"));
            out.current.effort = text(value.get("defaultThinkingLevel"));
            if let Some(providers) = value
                .get("providers")
                .and_then(Value::as_object)
                .or_else(|| value.as_object())
            {
                for (pid, p) in providers {
                    if let Some(models) = p.get("models").and_then(Value::as_array) {
                        for m in models {
                            if let Some(id) = text(m.get("id")) {
                                out.models.push(model(
                                    Some(pid.clone()),
                                    id,
                                    m,
                                    keys(m.get("thinkingLevelMap")),
                                    None,
                                ));
                            }
                        }
                    }
                }
            }
        }
        "opencode" | "mimo" => {
            set_compound(
                &mut out.current,
                text(value.get("model")),
                driver == "opencode",
            );
            out.current.effort = text(value.get("variant"));
            if let Some(providers) = value.get("provider").and_then(Value::as_object) {
                for (pid, p) in providers {
                    if let Some(models) = p.get("models").and_then(Value::as_object) {
                        for (id, m) in models {
                            out.models.push(model(
                                Some(pid.clone()),
                                id.clone(),
                                m,
                                keys(m.get("variants")),
                                None,
                            ));
                        }
                    }
                }
            }
        }
        "codex" => {
            out.current.model = text(value.get("model"));
            out.current.effort = text(value.get("model_reasoning_effort"));
            if let Some(models) = value.get("models").and_then(Value::as_array) {
                for m in models {
                    if let Some(id) = text(m.get("slug")) {
                        let efforts = m
                            .get("supported_reasoning_levels")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(|v| text(v.get("effort"))).collect())
                            .unwrap_or_default();
                        out.models.push(model(
                            None,
                            id,
                            m,
                            efforts,
                            text(m.get("default_reasoning_level")),
                        ));
                    }
                }
            }
        }
        "claude" => {
            out.provider_route=claude_provider_route(&value);
            out.current.model = text(value.get("model"));
            let (value, unresolved) =
                claude_effort(&[claude_layer(&value)], out.current.model.as_deref());
            out.current.effort = value;
            if unresolved {
                out.diagnostics
                    .push("native_effort_alias_resolution_delegated".into());
            }
        }
        "codebuddy" => {
            out.current.model = text(value.get("model"));
            out.current.effort = text(value.get("reasoningEffort"));
        }
        "dsh" => {
            let m = value.get("agent-default-model").unwrap_or(&Value::Null);
            out.current.provider = text(m.get("provider"));
            out.current.model = text(m.get("model"));
            out.current.effort = text(m.get("reasoningEffort"));
        }
        "smartclaw" => {
            out.current.model = text(value.get("model"));
        }
        "cursor" | "agy" => {}
        _ => bail!("unsupported_native_config_driver"),
    }
    finish(out)
}
fn bounded_file(path: &Path) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.file_type().is_symlink() || before.len() > MAX_BYTES as u64 {
        bail!("unsafe_or_oversize_native_file");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(NO_FOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    let same = |a: &fs::Metadata, b: &fs::Metadata| {
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && a.mode() == b.mode()
    };
    if !opened.is_file() || !same(&before, &opened) {
        bail!("native_file_changed_before_read");
    }
    let mut data = Vec::new();
    (&file).take(MAX_BYTES as u64 + 1).read_to_end(&mut data)?;
    let after = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if data.len() > MAX_BYTES || !same(&opened, &after) || !same(&after, &named) || !named.is_file()
    {
        bail!("native_file_changed_during_read");
    }
    Ok(data)
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
fn installed(context: &DiscoveryContext, driver: HarnessId) -> Option<PathBuf> {
    let names: Vec<&str> = match driver {
        HarnessId::Cursor => vec!["cursor-agent"],
        HarnessId::SmartClaw => vec!["DewuSmartClaw", "smartclaw"],
        HarnessId::Dclaw => vec![],
        other => vec![other.as_str()],
    };
    for dir in &context.search_path {
        for name in &names {
            let p = dir.join(name);
            if p.is_absolute() && executable(&p) {
                return Some(p);
            }
        }
    }
    for name in &names {
        let path=context.home.join(".local/bin").join(name);
        if executable(&path) {return Some(path);}
    }
    if driver==HarnessId::Codex && context.include_platform_locations {
        let path=PathBuf::from("/Applications/Codex.app/Contents/Resources/codex");
        if executable(&path) {return Some(path);}
    }
    let extra = match driver {
        HarnessId::Codex => Some(context.home.join(".local/bin/codex")),
        HarnessId::Claude => Some(context.home.join(".local/bin/claude")),
        HarnessId::OpenCode => Some(context.home.join(".opencode/bin/opencode")),
        HarnessId::Mimo => Some(context.home.join(".mimocode/bin/mimo")),
        HarnessId::Agy => Some(context.home.join(".local/bin/agy")),
        HarnessId::SmartClaw if context.include_platform_locations => Some(PathBuf::from(
            "/Applications/DewuSmartClaw.app/Contents/MacOS/DewuSmartClaw",
        )),
        HarnessId::ZCode if context.include_platform_locations => Some(PathBuf::from(
            "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs",
        )),
        _ => None,
    };
    extra.filter(|p| executable(p))
}
/// Shared executable-only discovery for setup; no native metadata command is run.
pub(crate) fn installed_executables(context: &DiscoveryContext) -> Vec<(String,PathBuf)> {
    HarnessId::ALL.iter().filter_map(|driver|installed(context,*driver).and_then(|path|fs::canonicalize(path).ok()).map(|path|(driver.as_str().to_string(),path))).collect()
}
fn files(context: &DiscoveryContext, driver: &str) -> Vec<PathBuf> {
    let h = &context.home;
    let p = &context.project;
    let location = |key: &str, fallback: PathBuf| {
        context
            .overrides
            .get(key)
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    p.join(path)
                }
            })
            .unwrap_or(fallback)
    };
    let config = location("XDG_CONFIG_HOME", h.join(".config"));
    match driver {
        "codex" => vec![
            location("CODEX_HOME", h.join(".codex")).join("config.toml"),
            p.join(".codex/config.toml"),
        ],
        "claude" => vec![
            location("CLAUDE_CONFIG_DIR", h.join(".claude")).join("settings.json"),
            p.join(".claude/settings.json"),
            p.join(".claude/settings.local.json"),
        ],
        "codebuddy" => vec![
            h.join(".codebuddy/settings.json"),
            p.join(".codebuddy/settings.json"),
            p.join(".codebuddy/settings.local.json"),
        ],
        "pi" => {
            let dir = location("PI_CODING_AGENT_DIR", h.join(".pi/agent"));
            vec![
                dir.join("models-store.json"),
                dir.join("models.json"),
                dir.join("settings.json"),
                p.join(".pi/settings.json"),
            ]
        }
        "opencode" => vec![
            config.join("opencode/opencode.json"),
            config.join("opencode/opencode.jsonc"),
            p.join("opencode.json"),
            p.join("opencode.jsonc"),
        ],
        "mimo" => vec![
            config.join("mimocode/mimocode.json"),
            config.join("mimocode/mimocode.jsonc"),
            p.join("mimocode.json"),
            p.join("mimocode.jsonc"),
        ],
        "zcode" => vec![
            location("ORCH_ZCODE_CONFIG", h.join(".zcode/cli/config.json")),
            p.join("zcode.json"),
            p.join(".zcode/config.json"),
        ],
        "dsh" => vec![location("DSH_HOME", h.join(".dsh")).join("settings.yaml")],
        "smartclaw" => vec![h.join("Library/Application Support/DewuSmartClaw/api-config.json")],
        _ => vec![],
    }
}

fn json_objects(input: &str) -> Vec<Value> {
    let (mut start, mut depth, mut quoted, mut escaped) = (0, 0usize, false, false);
    let mut out = Vec::new();
    for (i, c) in input.char_indices() {
        if quoted {
            if escaped {
                escaped = false
            } else if c == '\\' {
                escaped = true
            } else if c == '"' {
                quoted = false
            }
            continue;
        }
        if c == '"' {
            quoted = true;
            continue;
        }
        if c == '{' {
            if depth == 0 {
                start = i;
            }
            depth += 1;
        } else if c == '}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                if let Ok(v) = serde_json::from_str(&input[start..=i]) {
                    out.push(v);
                }
            }
        }
    }
    out
}
/// Parse native catalog output, keeping only IDs, display names and effort names.
pub fn parse_catalog_output(driver: &str, input: &str) -> Result<Vec<NativeModel>> {
    if input.len() > MAX_BYTES {
        bail!("catalog_too_large");
    }
    if driver == "codex" {
        return Ok(parse_native_config(driver, input)?.models);
    }
    let mut models = Vec::new();
    if driver == "opencode" || driver == "mimo" {
        for value in json_objects(input) {
            if let (Some(id), Some(provider)) =
                (text(value.get("id")), text(value.get("providerID")))
            {
                models.push(model(
                    Some(provider),
                    id,
                    &value,
                    keys(value.get("variants")),
                    None,
                ));
            }
        }
        if models.is_empty() {
            for line in input.lines() {
                if !line.chars().any(char::is_whitespace) {
                    if let Some((p, m)) = line.split_once('/') {
                        if !p.is_empty() && !m.is_empty() {
                            models.push(model(
                                Some(p.into()),
                                m.into(),
                                &Value::Null,
                                vec![],
                                None,
                            ));
                        }
                    }
                }
            }
        }
    } else if driver == "agy" {
        for line in input.lines() {
            if let Some((id, name)) = line.split_once('\t') {
                if text(Some(&Value::String(id.into()))).is_some() {
                    models.push(NativeModel {
                        provider: Some("antigravity".into()),
                        id: id.into(),
                        name: name.into(),
                        efforts: vec![],
                        default_effort: None,
                    });
                }
            }
        }
    } else if driver == "codebuddy" {
        if let Some((_, tail)) = input.split_once("Currently supported:") {
            let list = tail.split(')').next().unwrap_or("");
            for id in list.split(',').map(str::trim) {
                if !id.is_empty() && !id.chars().any(char::is_whitespace) {
                    models.push(model(None, id.into(), &Value::Null, vec![], None));
                }
            }
        }
    } else if driver == "cursor" {
        for line in input.lines() {
            let line = line.trim();
            let mut parts = line.split_whitespace();
            if let Some(id) = parts.next() {
                if line.contains(" - ") && id != "Error:" {
                    models.push(model(None, id.into(), &Value::Null, vec![], None));
                }
            }
        }
    }
    let out = finish(NativeConfiguration {
        models,
        ..Default::default()
    })?;
    Ok(out.models)
}
fn remove_owned_scratch(path: &Path, expected: &fs::Metadata) -> Result<()> {
    let actual = fs::symlink_metadata(path)?;
    if !actual.is_dir()
        || actual.file_type().is_symlink()
        || actual.dev() != expected.dev()
        || actual.ino() != expected.ino()
    {
        bail!("query_directory_identity_changed");
    }
    fs::remove_dir_all(path)?;
    Ok(())
}
struct Query {
    output: String,
    status: String,
}
fn query(context: &DiscoveryContext, exe: &Path, args: &[&str], isolated: bool) -> Result<Query> {
    let main = crate::fusion_roles::project_root(&context.project)?;
    crate::fusion_roles::ensure_ignored(&main)?;
    let parent = main.join(".orch/native-discovery");
    if fs::symlink_metadata(main.join(".orch")).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("unsafe_scan_directory");
    }
    fs::create_dir_all(&parent)?;
    if fs::symlink_metadata(&parent)?.file_type().is_symlink() {
        bail!("unsafe_scan_directory");
    }
    let scratch = parent.join(ulid::Ulid::new().to_string());
    fs::create_dir(&scratch)?;
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
    let stdout = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(scratch.join("stdout"))?;
    let stderr = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(scratch.join("stderr"))?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .current_dir(&context.project)
        .env("HOME", &context.home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    for key in CONFIG_OVERRIDES {
        cmd.env_remove(key);
    }
    cmd.envs(&context.overrides);
    cmd.env("PATH", std::env::join_paths(&context.search_path)?);
    // A new session prevents unrelated processes in the caller's session from
    // joining this query group. The unreaped child reserves its PID at timeout.
    unsafe {
        cmd.pre_exec(|| {
            unsafe extern "C" {
                fn setsid() -> i32;
            }
            if setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if isolated {
        cmd.current_dir(&scratch)
            .env("XDG_DATA_HOME", scratch.join("data"))
            .env("XDG_CACHE_HOME", scratch.join("cache"))
            .env("XDG_STATE_HOME", scratch.join("state"));
    }
    let scratch_identity = fs::metadata(&scratch)?;
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            remove_owned_scratch(&scratch, &scratch_identity)?;
            return Err(error).context("native_query_start_failed");
        }
    };
    let group = child.id() as i32;
    let start = std::time::Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.wait_timeout(Duration::from_millis(25))? {
            break status;
        }
        if start.elapsed() >= Duration::from_millis(context.query_timeout_ms.clamp(1, 10000))
            || stdout.metadata()?.len() > MAX_BYTES as u64
            || stderr.metadata()?.len() > MAX_BYTES as u64
        {
            timed_out = true;
            unsafe extern "C" {
                fn killpg(group: i32, signal: i32) -> i32;
            }
            unsafe {
                killpg(group, 9);
            }
            break child.wait()?;
        }
    };
    let read = |mut file: fs::File| -> Result<String> {
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        let mut b = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut b)?;
        if b.len() > MAX_BYTES {
            bail!("native_query_output_limit");
        }
        Ok(String::from_utf8_lossy(&b).into_owned())
    };
    let output = read(stdout);
    let errors = read(stderr);
    unsafe extern "C" {
        fn killpg(group: i32, signal: i32) -> i32;
    }
    let reap_start = std::time::Instant::now();
    let ended = loop {
        if unsafe { killpg(group, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(3)
        {
            break true;
        }
        if reap_start.elapsed() > Duration::from_millis(200) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if ended {
        remove_owned_scratch(&scratch, &scratch_identity)?;
    }
    if !ended {
        return Ok(Query {
            output: String::new(),
            status: "query-scope-unknown".into(),
        });
    }
    if timed_out {
        return Ok(Query {
            output: String::new(),
            status: "query-timeout-or-output-limit".into(),
        });
    }
    let output = output?;
    let errors = errors?;
    if !status.success() {
        let lower = errors.to_lowercase() + &output.to_lowercase();
        return Ok(Query {
            output: String::new(),
            status: if lower.contains("authentication required")
                || lower.contains("not logged in")
                || lower.contains("unauthorized")
            {
                "auth-required"
            } else {
                "query-failed"
            }
            .into(),
        });
    }
    Ok(Query {
        output,
        status: "ok".into(),
    })
}
fn dsh_settings_path(context: &DiscoveryContext, profile: &str) -> Result<PathBuf> {
    let value: serde_yaml::Value = serde_yaml::from_str(profile)?;
    let rows = value.as_sequence().context("native_profile_not_sequence")?;
    let matches: Vec<_> = rows
        .iter()
        .filter(|v| v.get("id").and_then(serde_yaml::Value::as_str) == Some("settings"))
        .collect();
    if matches.len() != 1
        || matches[0]
            .get("disabled")
            .and_then(serde_yaml::Value::as_bool)
            == Some(true)
    {
        bail!("native_settings_entry_unavailable");
    }
    let settings = matches[0].get("config");
    if settings.and_then(|v| v.get("dshHome")).is_some() {
        bail!("native_settings_home_expression_unsupported");
    }
    match settings.and_then(|v| v.get("path")) {
        None => Ok(files(context, "dsh")[0].clone()),
        Some(value) => {
            let path = PathBuf::from(
                value
                    .as_str()
                    .context("native_settings_path_expression_unsupported")?,
            );
            Ok(if path.is_absolute() {
                path
            } else {
                context.project.join(path)
            })
        }
    }
}
fn project_native(
    context: &DiscoveryContext,
    driver: &str,
    exe: &Path,
    selected_profile: Option<&str>,
) -> (NativeConfiguration, Vec<NativeSource>) {
    let mut out = NativeConfiguration::default();
    let mut sources = Vec::new();
    let mut effort_layers = Vec::new();
    for path in files(context, driver) {
        if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            continue;
        }
        match bounded_file(&path).and_then(|bytes| {
            let input = std::str::from_utf8(&bytes).context("native_file_not_utf8")?;
            let parsed = parse_native_config(driver, input)?;
            if driver == "claude" {
                let value: Value = serde_json::from_str(&jsonc_text(input)?)?;
                effort_layers.push(claude_layer(&value));
            }
            Ok((
                parsed,
                NativeSource {
                    kind: if path.file_name().is_some_and(|n| n == "models-store.json") {
                        "native-cache"
                    } else {
                        "native-config"
                    }
                    .into(),
                    path: path.display().to_string(),
                    sha256: hex::encode(Sha256::digest(&bytes)),
                },
            ))
        }) {
            Ok((parsed, source)) => {
                out.current = crate::fusion_roles::resolve_tuple(&out.current, &parsed.current);
                out.provider_route=parsed.provider_route.or(out.provider_route);
                out.models.extend(parsed.models);
                out.diagnostics.extend(parsed.diagnostics);
                sources.push(source);
            }
            Err(_) => out
                .diagnostics
                .push(format!("{driver}: native_config_unreadable_or_unsupported")),
        }
    }
    if driver == "claude" {
        if let Some(value) = context.overrides.get("ANTHROPIC_MODEL") {
            out.current.model = Some(value.clone());
            sources.push(NativeSource {
                kind: "environment".into(),
                path: "ANTHROPIC_MODEL".into(),
                sha256: hex::encode(Sha256::digest(value.as_bytes())),
            });
        }
        let (value, unresolved) = claude_effort(&effort_layers, out.current.model.as_deref());
        out.current.effort = value;
        out.diagnostics
            .retain(|d| d != "native_effort_alias_resolution_delegated");
        if let Some(value) = context.overrides.get("CLAUDE_CODE_EFFORT_LEVEL") {
            out.current.effort = Some(value.clone());
            sources.push(NativeSource {
                kind: "environment".into(),
                path: "CLAUDE_CODE_EFFORT_LEVEL".into(),
                sha256: hex::encode(Sha256::digest(value.as_bytes())),
            });
        } else if unresolved {
            out.diagnostics
                .push("native_effort_alias_resolution_delegated".into());
        }
    }
    if context.allow_commands && driver == "dsh" {
        let profile = selected_profile
            .or_else(|| {
                context
                    .overrides
                    .get("ORCH_DSH_PROFILE")
                    .map(String::as_str)
            })
            .unwrap_or("headless");
        let result = (|| -> Result<(NativeConfiguration, Vec<NativeSource>)> {
            let q = query(
                context,
                exe,
                &["--profile", profile, "--dump-config"],
                false,
            )?;
            if q.status != "ok" {
                bail!("native_profile_unavailable");
            }
            let path = dsh_settings_path(context, &q.output)?;
            let bytes = bounded_file(&path)?;
            let parsed = parse_native_config("dsh", std::str::from_utf8(&bytes)?)?;
            Ok((
                parsed,
                vec![
                    NativeSource {
                        kind: "native-command".into(),
                        path: format!("dsh --profile {profile} --dump-config"),
                        sha256: hex::encode(Sha256::digest(q.output.as_bytes())),
                    },
                    NativeSource {
                        kind: "native-config".into(),
                        path: path.display().to_string(),
                        sha256: hex::encode(Sha256::digest(&bytes)),
                    },
                ],
            ))
        })();
        match result {
            Ok((parsed, found)) => {
                out.current = parsed.current;
                out.current.mode = Some(profile.to_owned());
                sources = found;
            }
            Err(_) => {
                out.diagnostics.push("native_profile_unavailable".into());
            }
        }
    }
    if context.allow_commands {
        let args: Option<&[&str]> = match driver {
            "codex" => Some(&["debug", "models", "--bundled"]),
            "opencode" | "mimo" => Some(&["models", "--verbose"]),
            "codebuddy" => Some(&["--help"]),
            "cursor" | "agy" => Some(&["models"]),
            _ => None,
        };
        if let Some(args) = args {
            match query(context, exe, args, matches!(driver, "opencode" | "mimo")) {
                Ok(q) if q.status == "ok" => {
                    sources.push(NativeSource {
                        kind: if driver == "codex" {
                            "bundled"
                        } else if driver == "codebuddy" {
                            "help-derived"
                        } else {
                            "native-command"
                        }
                        .into(),
                        path: format!("{} {}", driver, args.join(" ")),
                        sha256: hex::encode(Sha256::digest(q.output.as_bytes())),
                    });
                    match parse_catalog_output(driver, &q.output) {
                        Ok(models) => {
                            if models.is_empty() {
                                out.diagnostics.push("native_catalog_not_exposed".into());
                            }
                            out.models.extend(models);
                        }
                        Err(_) => out
                            .diagnostics
                            .push("native_catalog_format_unavailable".into()),
                    }
                }
                Ok(q) => {
                    out.diagnostics.push(q.status.clone());
                    if q.status == "auth-required" {
                        out.status = q.status;
                    }
                }
                Err(_) => out.diagnostics.push("native_query_unavailable".into()),
            }
        }
    }
    let auth = out.status == "auth-required";
    let config_error = out.diagnostics.iter().any(|d| {
        d.contains("native_config_unreadable_or_unsupported")
            || d.contains("native_profile_unavailable")
    });
    let diagnostics = out.diagnostics.clone();
    let mut out = finish(out).unwrap_or_else(|_| NativeConfiguration {
        diagnostics,
        ..Default::default()
    });
    if driver == "zcode" {
        out.current.effort = out
            .models
            .iter()
            .find(|m| {
                m.provider == out.current.provider && Some(&m.id) == out.current.model.as_ref()
            })
            .and_then(|m| m.default_effort.clone());
    }
    if config_error {
        out.provider_route=None;
        out.current = InvocationTuple::default();
        out.status = "unavailable".into();
    } else if auth {
        out.status = "auth-required".into();
    }
    (out, sources)
}
/// Scan actual local clients without inference or changing native settings.
pub fn discover(project: &Path) -> Result<DiscoverySnapshot> {
    discover_with_context(&DiscoveryContext::system(project)?)
}
/// Scan a captured environment; fake executables/configs can exercise the real IO path.
pub fn discover_with_context(context: &DiscoveryContext) -> Result<DiscoverySnapshot> {
    discover_captured(context, None)
}
/// Discover native metadata using the caller's already captured configuration.
/// The project harness file is not read or parsed a second time.
pub(crate) fn discover_with_config(context: &DiscoveryContext, snapshot: HarnessConfigSnapshot) -> Result<DiscoverySnapshot> {
    discover_captured(context, Some(snapshot))
}
fn discover_captured(context: &DiscoveryContext, mut captured: Option<HarnessConfigSnapshot>) -> Result<DiscoverySnapshot> {
    let main = crate::fusion_roles::project_root(&context.project)?;
    let mut result = DiscoverySnapshot {
        observed_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        harnesses: Vec::new(),
        diagnostics: Vec::new(),
        configured_snapshot: None,
    };
    let config_path = main.join(".orch/harnesses.yaml");
    if captured.is_some() || fs::symlink_metadata(&config_path).is_ok() {
        let loaded = if let Some(snapshot) = captured.take() { Ok(snapshot) } else { (|| -> Result<HarnessConfigSnapshot> {
            if fs::symlink_metadata(main.join(".orch"))?
                .file_type()
                .is_symlink()
            {
                bail!("unsafe_config_parent");
            }
            if !Command::new("git")
                .args(["check-ignore", "-q", ".orch/harnesses.yaml"])
                .current_dir(&main)
                .status()?
                .success()
            {
                bail!("config_not_ignored");
            }
            let bytes = bounded_file(&config_path)?;
            crate::harness_config::parse_harness_config_snapshot(
                &config_path,
                std::str::from_utf8(&bytes).context("config_not_utf8")?,
            )
        })() };
        match loaded {
            Ok(snapshot) => {
                for row in snapshot.discover_for_action(HarnessAction::Consult) {
                    let resolved = snapshot
                        .inspect_configured(row.alias(), HarnessAction::Consult)
                        .ok();
                    result.harnesses.push(NativeHarness {
                        backend: if resolved.as_ref().is_some_and(|r|r.acp().is_some()) {"acp"}else{"native"}.into(),
                        id: format!("configured:{}", row.alias()),
                        driver: row.driver().into(),
                        executable: resolved.as_ref().map(|r| r.executable().to_path_buf()),
                        availability: row.availability().label().into(),
                        enabled: snapshot.configured_enabled(row.alias()).unwrap_or(false),
                        reason: row.availability().reason().map(str::to_owned),
                        alias: Some(row.alias().into()),
                        configured: resolved
                            .as_ref()
                            .map(|r| InvocationTuple {
                                provider: r.provider().map(str::to_owned),
                                model: r.model().map(str::to_owned),
                                effort: r.effort().map(str::to_owned),
                                mode: r.mode().map(str::to_owned),
                            })
                            .unwrap_or_default(),
                        native: NativeConfiguration::default(),
                        sources: vec![],
                    });
                }
                result.configured_snapshot = Some(snapshot);
            }
            Err(_) => result
                .diagnostics
                .push("project_harness_config_invalid".into()),
        }
    }
    for driver in HarnessId::ALL {
        if let Some(exe) = installed(context, *driver) {
            result.harnesses.push(NativeHarness {
                backend: "native".into(),
                id: format!("installed:{}", driver.as_str()),
                driver: driver.as_str().into(),
                executable: Some(exe),
                availability: if driver.supports_action(DriverAction::Consult) {
                    "supported"
                } else {
                    "unsupported"
                }
                .into(),
                enabled: true,
                reason: if driver.supports_action(DriverAction::Consult) {
                    None
                } else {
                    Some("consult_not_supported".into())
                },
                alias: None,
                configured: InvocationTuple::default(),
                native: NativeConfiguration::default(),
                sources: vec![],
            });
        }
    }
    let mut cache: BTreeMap<
        (String, PathBuf, Option<String>),
        (NativeConfiguration, Vec<NativeSource>),
    > = BTreeMap::new();
    for row in &mut result.harnesses {
        if !row.enabled {
            continue;
        }
        if let Some(exe) = &row.executable {
            let profile = if row.driver == "dsh" {
                row.configured.mode.clone()
            } else {
                None
            };
            let key = (row.driver.clone(), exe.clone(), profile.clone());
            let data = cache
                .entry(key)
                .or_insert_with(|| project_native(context, &row.driver, exe, profile.as_deref()));
            row.native = data.0.clone();
            row.sources = data.1.clone();
        }
    }
    Ok(result)
}

#[cfg(test)]
mod helper_locator_tests {
    use super::*;
    #[test]
    fn shared_locator_preserves_legacy_local_bin_names_without_execution() {
        let root=crate::util::test_scratch_dir("helper executable locator");let bin=root.join(".local/bin");fs::create_dir_all(&bin).unwrap();
        for name in ["smartclaw","cursor-agent","codebuddy"] {let path=bin.join(name);fs::write(&path,"#!/bin/sh\nexit 99\n").unwrap();fs::set_permissions(&path,fs::Permissions::from_mode(0o755)).unwrap();}
        let context=DiscoveryContext{project:root.clone(),home:root.clone(),search_path:vec![],query_timeout_ms:1,allow_commands:false,overrides:Default::default(),include_platform_locations:false};
        let rows=installed_executables(&context);for expected in ["smartclaw","cursor","codebuddy"] {assert!(rows.iter().any(|(name,_)|name==expected));}
    }
}
