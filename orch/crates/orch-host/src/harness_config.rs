//! Git-ignored harness configuration and token-free capability discovery.
//!
//! Configuration selects a code-owned driver and supplies only local pins. It
//! never carries argv, environment, transport, receipt, terminal, or control
//! truth. A caller loads one immutable snapshot and passes that snapshot through
//! the rest of an action.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::harness::{DriverAction, HarnessId};

/// Version of the local harness-configuration contract.
pub const HARNESS_CONFIG_CONTRACT_V1: u32 = 1;

/// Repository-relative location of the local, git-ignored configuration.
pub const HARNESS_CONFIG_RELPATH: &str = ".orch/harnesses.yaml";

/// Invocation class whose tuple override should be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessAction {
    /// Task implementation or another direct execution request.
    Execute,
    /// Fixed-HEAD code review.
    Review,
    /// Explicit-member consultation.
    Consult,
}

impl HarnessAction {
    /// Return the stable configuration spelling for this action.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Review => "review",
            Self::Consult => "consult",
        }
    }
}

/// Working-directory selection made by local configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessCwdPolicy {
    /// Run in the canonical main-project root.
    ProjectRoot,
    /// Run in the target task or review worktree.
    TargetWorktree,
}

impl HarnessCwdPolicy {
    /// Return the stable configuration spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectRoot => "project-root",
            Self::TargetWorktree => "target-worktree",
        }
    }
}

/// Token-free availability classification for one configured alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessAvailability {
    /// The local executable, driver, action, and tuple are usable.
    Supported,
    /// The row is understood but cannot currently service the request.
    Unsupported(String),
    /// The row names a driver that this binary does not understand.
    Unknown(String),
}

impl HarnessAvailability {
    /// Return the stable human-readable status word.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported(_) => "unsupported",
            Self::Unknown(_) => "unknown",
        }
    }

    /// Return the diagnostic reason for a non-supported row.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Unknown(reason) => Some(reason),
        }
    }
}

/// Provider/model/effort/mode pins after applying an action override.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessTuple {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
}

impl HarnessTuple {
    /// Requested provider pin, if configured.
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Requested model pin, if configured.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Requested reasoning-effort pin, if configured.
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Requested driver mode, if configured.
    pub fn mode(&self) -> Option<&str> {
        self.mode.as_deref()
    }
}

/// Validated action-local limits. Missing fields preserve the existing default;
/// a complete prompt and an effective deadline are checked before provider spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessInvocationLimits {
    max_prompt_bytes: Option<usize>,
    timeout_seconds: Option<u64>,
}

impl HarnessInvocationLimits {
    /// Optional maximum size of the complete model prompt in UTF-8 bytes.
    pub fn max_prompt_bytes(&self) -> Option<usize> {
        self.max_prompt_bytes
    }

    /// Action-local deadline preference, overridden by an explicit CLI value.
    pub fn timeout_seconds(&self) -> Option<u64> {
        self.timeout_seconds
    }

    /// Refuse an oversized complete prompt without truncating its input.
    pub fn validate_prompt(&self, prompt: &str) -> Result<()> {
        if let Some(cap) = self.max_prompt_bytes {
            if prompt.len() > cap {
                bail!("rendered prompt exceeds maxPromptBytes: bytes={} cap={cap}", prompt.len());
            }
        }
        Ok(())
    }

    /// Select explicit CLI, then action config, then workload default, and
    /// apply the caller's existing hard ceiling. Zero is never an unbounded SLA.
    pub fn deadline_seconds(
        &self,
        explicit: Option<u64>,
        workload_default: u64,
        hard_ceiling: u64,
    ) -> Result<u64> {
        if hard_ceiling == 0 {
            bail!("invocation hard ceiling must be positive");
        }
        let requested = explicit.or(self.timeout_seconds).unwrap_or(workload_default);
        if requested == 0 {
            bail!("invocation deadline must be positive");
        }
        Ok(requested.min(hard_ceiling))
    }
}

/// One action-specific alias resolved entirely from an immutable snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHarness {
    alias: String,
    driver: HarnessId,
    executable: PathBuf,
    tuple: HarnessTuple,
    limits: HarnessInvocationLimits,
    cwd_policy: HarnessCwdPolicy,
    config_digest: String,
    source_path: PathBuf,
}

impl ResolvedHarness {
    /// Configured alias used to select this invocation.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Code-owned driver selected by the alias.
    pub fn driver(&self) -> HarnessId {
        self.driver
    }

    /// Absolute executable path captured by the snapshot.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Effective provider pin after action override.
    pub fn provider(&self) -> Option<&str> {
        self.tuple.provider()
    }

    /// Effective model pin after action override.
    pub fn model(&self) -> Option<&str> {
        self.tuple.model()
    }

    /// Effective reasoning-effort pin after action override.
    pub fn effort(&self) -> Option<&str> {
        self.tuple.effort()
    }

    /// Effective driver mode after action override.
    pub fn mode(&self) -> Option<&str> {
        self.tuple.mode()
    }

    /// Effective tuple as an immutable value.
    pub fn effective_tuple(&self) -> &HarnessTuple {
        &self.tuple
    }

    /// Validated limits belonging only to this selected action and snapshot.
    pub fn limits(&self) -> &HarnessInvocationLimits {
        &self.limits
    }

    /// Working-directory policy captured for the action.
    pub fn cwd_policy(&self) -> HarnessCwdPolicy {
        self.cwd_policy
    }

    /// SHA-256 of the exact configuration bytes used for this action.
    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    /// Absolute configuration path from which the snapshot was loaded.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }
}

/// One deterministic row returned by token-free discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessDiscoveryRow {
    alias: String,
    driver: String,
    availability: HarnessAvailability,
}

impl HarnessDiscoveryRow {
    /// Configured alias in lexical order.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Raw driver spelling, retained even when this binary does not know it.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// Token-free availability result captured with the snapshot.
    pub fn availability(&self) -> &HarnessAvailability {
        &self.availability
    }
}

/// Exact local configuration bytes plus their parsed, immutable row state.
#[derive(Debug, Clone)]
pub struct HarnessConfigSnapshot {
    source_path: PathBuf,
    source_bytes: Arc<[u8]>,
    sha256: String,
    entries: BTreeMap<String, HarnessEntry>,
}

impl HarnessConfigSnapshot {
    /// SHA-256 of the original file bytes without YAML normalization.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Exact bytes captured during the single read.
    pub fn source_bytes(&self) -> &[u8] {
        &self.source_bytes
    }

    /// Absolute source path bound to this snapshot.
    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    /// Resolve one selected alias and action without touching disk or process state.
    /// Tuple defaults may be inherited; limits belong only to this action.
    pub fn resolve(&self, alias: &str, action: HarnessAction) -> Result<ResolvedHarness> {
        let entry = self
            .entries
            .get(alias)
            .with_context(|| format!("harness alias {alias:?} 不存在"))?;
        match availability_for(entry, Some(action)) {
            HarnessAvailability::Supported => {}
            HarnessAvailability::Unsupported(reason) | HarnessAvailability::Unknown(reason) => {
                bail!(
                    "harness alias {alias:?} {} 不可用: {reason}",
                    action.as_str()
                )
            }
        }
        self.inspect_configured(alias, action)
    }

    /// Inspect captured configuration without claiming that the requested action is available.
    /// This metadata-only path never grants permission to invoke a disabled/unsupported alias;
    /// execution must continue to use resolve and its action availability checks.
    pub(crate) fn inspect_configured(
        &self,
        alias: &str,
        action: HarnessAction,
    ) -> Result<ResolvedHarness> {
        let entry = self
            .entries
            .get(alias)
            .with_context(|| format!("harness alias {alias:?} 不存在"))?;
        let driver = HarnessId::parse(&entry.driver)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("解析 harness alias {alias:?} driver 失败"))?;
        Ok(ResolvedHarness {
            alias: alias.to_string(),
            driver,
            executable: entry.executable.clone(),
            tuple: entry.effective_tuple(action),
            limits: entry.invocation_limits(action),
            cwd_policy: entry.cwd_policy,
            config_digest: self.sha256.clone(),
            source_path: self.source_path.clone(),
        })
    }

    /// Read the captured enabled flag without executing or rereading configuration.
    pub(crate) fn configured_enabled(&self, alias: &str) -> Option<bool> {
        self.entries.get(alias).map(|entry| entry.enabled)
    }

    // The browser supplies only role IDs and parameter fields. Bind executable,
    // enabled/action support and limits from the server's captured discovery.
    pub(crate) fn for_fusion(
        source_path: &Path,
        discovery: &crate::native_discovery::DiscoverySnapshot,
        roles: &[(
            String,
            crate::fusion_roles::FusionRole,
            crate::channel::InvocationTuple,
        )],
        provenance: serde_json::Value,
    ) -> Result<Self> {
        if !source_path.is_absolute() {
            bail!("fusion_snapshot_path_must_be_absolute");
        }
        let mut entries = BTreeMap::new();
        for (alias, role, tuple) in roles {
            validate_alias(alias)?;
            crate::fusion_roles::validate_tuple(tuple)?;
            let Some(row) = discovery.harnesses.iter().find(|h| h.id == role.harness) else {
                continue;
            };
            let Some(executable) = row.executable.clone() else {
                continue;
            };
            let existing = row
                .alias
                .as_ref()
                .and_then(|id| {
                    discovery
                        .configured_snapshot
                        .as_ref()
                        .and_then(|s| s.entries.get(id))
                })
                .cloned();
            let mut entry = existing.unwrap_or_else(|| HarnessEntry {
                driver: row.driver.clone(),
                executable: executable.clone(),
                enabled: row.enabled,
                defaults: HarnessTuple::default(),
                execute: None,
                review: None,
                consult: None,
                cwd_policy: HarnessCwdPolicy::ProjectRoot,
                executable_availability: inspect_executable(&executable),
            });
            let limits = entry.invocation_limits(HarnessAction::Consult);
            entry.enabled = row.enabled && row.availability == "supported";
            entry.defaults = HarnessTuple {
                provider: tuple.provider.clone(),
                model: tuple.model.clone(),
                effort: tuple.effort.clone(),
                mode: tuple.mode.clone(),
            };
            entry.execute = None;
            entry.review = None;
            entry.consult = Some(HarnessActionOverride {
                tuple: HarnessTuple::default(),
                limits,
            });
            if entries.insert(alias.clone(), entry).is_some() {
                bail!("duplicate_fusion_invocation_alias");
            }
        }
        let bindings = entries
            .iter()
            .map(|(alias, entry)| {
                serde_json::json!({
                    "alias": alias, "driver": entry.driver, "executable": entry.executable,
                    "enabled": entry.enabled, "cwdPolicy": format!("{:?}", entry.cwd_policy),
                    "limits": entry.invocation_limits(HarnessAction::Consult)
                })
            })
            .collect::<Vec<_>>();
        let upstream = discovery
            .configured_snapshot
            .as_ref()
            .map(HarnessConfigSnapshot::sha256);
        let bytes = serde_json::to_vec_pretty(
            &serde_json::json!({"version":1,"roles":roles,"bindings":bindings,"upstreamConfigSha256":upstream,"provenance":provenance}),
        )?;
        if bytes.len() > 2 * 1024 * 1024 {
            bail!("fusion_snapshot_too_large");
        }
        Ok(Self {
            source_path: source_path.to_path_buf(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            source_bytes: bytes.into(),
            entries,
        })
    }

    /// Return every alias independently without starting a process or probing a token.
    pub fn discover_without_tokens(&self) -> Vec<HarnessDiscoveryRow> {
        self.entries
            .iter()
            .map(|(alias, entry)| HarnessDiscoveryRow {
                alias: alias.clone(),
                driver: entry.driver.clone(),
                availability: availability_for(entry, None),
            })
            .collect()
    }

    /// Inspect every alias for one specific driver action without starting a
    /// provider. Missing override sections inherit tuple defaults, not ability.
    pub fn discover_for_action(&self, action: HarnessAction) -> Vec<HarnessDiscoveryRow> {
        self.entries.iter().map(|(alias, entry)| HarnessDiscoveryRow {
            alias: alias.clone(),
            driver: entry.driver.clone(),
            availability: availability_for(entry, Some(action)),
        }).collect()
    }

    /// Reject any non-supported row while retaining row-local diagnostics.
    pub fn lint_without_tokens(&self) -> Result<()> {
        let failures = self
            .discover_without_tokens()
            .into_iter()
            .filter_map(|row| {
                row.availability
                    .reason()
                    .map(|reason| format!("{}={}({reason})", row.alias, row.availability.label()))
            })
            .collect::<Vec<_>>();
        if !failures.is_empty() {
            bail!("harness config lint 失败: {}", failures.join("; "));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct HarnessEntry {
    driver: String,
    executable: PathBuf,
    enabled: bool,
    defaults: HarnessTuple,
    execute: Option<HarnessActionOverride>,
    review: Option<HarnessActionOverride>,
    consult: Option<HarnessActionOverride>,
    cwd_policy: HarnessCwdPolicy,
    executable_availability: HarnessAvailability,
}

impl HarnessEntry {
    fn action_override(&self, action: HarnessAction) -> Option<&HarnessActionOverride> {
        match action {
            HarnessAction::Execute => self.execute.as_ref(),
            HarnessAction::Review => self.review.as_ref(),
            HarnessAction::Consult => self.consult.as_ref(),
        }
    }

    fn invocation_limits(&self, action: HarnessAction) -> HarnessInvocationLimits {
        self.action_override(action).map(|value| value.limits.clone()).unwrap_or_default()
    }

    fn effective_tuple(&self, action: HarnessAction) -> HarnessTuple {
        let mut tuple = self.defaults.clone();
        if let Some(override_value) = self.action_override(action) {
            merge_tuple(&mut tuple, &override_value.tuple);
        }
        tuple
    }
}

#[derive(Debug, Clone)]
struct HarnessActionOverride {
    tuple: HarnessTuple,
    limits: HarnessInvocationLimits,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessConfigFile {
    version: u32,
    harnesses: BTreeMap<String, HarnessEntryFile>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HarnessEntryFile {
    driver: String,
    executable: PathBuf,
    enabled: bool,
    #[serde(default)]
    defaults: HarnessTupleFile,
    execute: Option<HarnessActionFile>,
    review: Option<HarnessActionFile>,
    consult: Option<HarnessActionFile>,
    cwd_policy: HarnessCwdPolicy,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessTupleFile {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessActionFile {
    provider: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mode: Option<String>,
    #[serde(default, deserialize_with = "present_option")]
    limits: Option<HarnessLimitsFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HarnessLimitsFile {
    #[serde(default, deserialize_with = "present_option")]
    max_prompt_bytes: Option<usize>,
    #[serde(default, deserialize_with = "present_option")]
    timeout_seconds: Option<u64>,
}

// Absent limits are compatible. An explicitly present null is not a number
// (or a limits object) and must not silently disable a configured guard.
fn present_option<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn validate_action(
    file: HarnessActionFile,
    alias: &str,
    action: &str,
) -> Result<HarnessActionOverride> {
    let raw = file.limits.unwrap_or_default();
    if raw.max_prompt_bytes == Some(0) || raw.timeout_seconds == Some(0) {
        bail!("harness {alias} {action}.limits values must be positive");
    }
    let tuple = validate_tuple(
        HarnessTupleFile {
            provider: file.provider,
            model: file.model,
            effort: file.effort,
            mode: file.mode,
        },
        alias,
        action,
    )?;
    Ok(HarnessActionOverride {
        tuple,
        limits: HarnessInvocationLimits {
            max_prompt_bytes: raw.max_prompt_bytes,
            timeout_seconds: raw.timeout_seconds,
        },
    })
}

/// Parse exact UTF-8 configuration bytes into one immutable snapshot.
///
/// This pure entry point deliberately does not access `source_path`; filesystem
/// callers must use [`load_harness_config_snapshot`] for no-symlink and
/// git-ignore enforcement.
/// Action-only limits reject unknown keys and explicitly invalid/null values;
/// omitted limits preserve compatibility with the earlier tuple-only schema.
pub fn parse_harness_config_snapshot(
    source_path: &Path,
    source_text: &str,
) -> Result<HarnessConfigSnapshot> {
    if !source_path.is_absolute() {
        bail!("harness config source path 必须是绝对路径");
    }
    let parsed: HarnessConfigFile = serde_yaml::from_str(source_text).map_err(|error| {
        anyhow::anyhow!(
            "解析 harness config 失败: {}: {error}",
            source_path.display()
        )
    })?;
    if parsed.version != HARNESS_CONFIG_CONTRACT_V1 {
        bail!(
            "harness config version={} 未建模，期望 {}",
            parsed.version,
            HARNESS_CONFIG_CONTRACT_V1
        );
    }
    if parsed.harnesses.is_empty() {
        bail!("harness config 至少需要一个 alias");
    }

    let mut entries = BTreeMap::new();
    for (alias, file) in parsed.harnesses {
        validate_alias(&alias)?;
        validate_exact_string(&file.driver, &format!("harness {alias} driver"))?;
        if !file.executable.is_absolute() {
            bail!("harness {alias} executable 必须是绝对路径，禁止 PATH lookup");
        }
        let defaults = validate_tuple(file.defaults, &alias, "defaults")?;
        let execute = file
            .execute
            .map(|value| validate_action(value, &alias, "execute"))
            .transpose()?;
        let review = file
            .review
            .map(|value| validate_action(value, &alias, "review"))
            .transpose()?;
        let consult = file
            .consult
            .map(|value| validate_action(value, &alias, "consult"))
            .transpose()?;
        let executable_availability = inspect_executable(&file.executable);
        entries.insert(
            alias,
            HarnessEntry {
                driver: file.driver,
                executable: file.executable,
                enabled: file.enabled,
                defaults,
                execute,
                review,
                consult,
                cwd_policy: file.cwd_policy,
                executable_availability,
            },
        );
    }

    let source_bytes: Arc<[u8]> = Arc::from(source_text.as_bytes());
    let sha256 = hex::encode(Sha256::digest(source_bytes.as_ref()));
    Ok(HarnessConfigSnapshot {
        source_path: source_path.to_path_buf(),
        source_bytes,
        sha256,
        entries,
    })
}

/// Locate the main repository from any linked worktree and capture the local
/// configuration once with no-symlink, regular-file, and git-ignore checks.
pub fn load_harness_config_snapshot(root: &Path) -> Result<HarnessConfigSnapshot> {
    let common_dir = crate::gitx::canonical_worktree_common_dir(root)
        .context("解析 harness config 的 git common dir 失败")?;
    let main_root = common_dir
        .parent()
        .context("git common dir 没有主仓 parent")?;
    let config_dir = main_root.join(".orch");
    let config_path = main_root.join(HARNESS_CONFIG_RELPATH);

    require_real_directory(&config_dir)?;
    let before = require_regular_no_symlink(&config_path)?;
    require_gitignored(main_root)?;

    let mut file = File::open(&config_path)
        .with_context(|| format!("打开 harness config 失败: {}", config_path.display()))?;
    let opened_before = file
        .metadata()
        .with_context(|| format!("fstat harness config 失败: {}", config_path.display()))?;
    require_same_file(&before, &opened_before, "open 前后")?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("单次读取 harness config 失败: {}", config_path.display()))?;
    let opened_after = file.metadata().with_context(|| {
        format!(
            "读取后 fstat harness config 失败: {}",
            config_path.display()
        )
    })?;
    let after = require_regular_no_symlink(&config_path)?;
    require_same_file(&opened_before, &opened_after, "读取期间")?;
    require_same_file(&opened_after, &after, "读取完成后")?;
    if opened_after.len() != bytes.len() as u64 {
        bail!("harness config 读取期间长度漂移");
    }
    let text = std::str::from_utf8(&bytes).context("harness config 不是 UTF-8")?;
    parse_harness_config_snapshot(&config_path, text)
}

fn validate_alias(alias: &str) -> Result<()> {
    let valid = !alias.is_empty()
        && alias
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        bail!("harness alias {alias:?} 不是安全 identity component");
    }
    Ok(())
}

fn validate_exact_string(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        bail!("{label} 必须是无首尾空白的非空字符串");
    }
    Ok(())
}

fn validate_tuple(file: HarnessTupleFile, alias: &str, scope: &str) -> Result<HarnessTuple> {
    for (field, value) in [
        ("provider", file.provider.as_deref()),
        ("model", file.model.as_deref()),
        ("effort", file.effort.as_deref()),
        ("mode", file.mode.as_deref()),
    ] {
        if let Some(value) = value {
            validate_exact_string(value, &format!("harness {alias} {scope}.{field}"))?;
        }
    }
    if let Some(effort) = file.effort.as_deref() {
        if !matches!(
            effort,
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
        ) {
            bail!("harness {alias} {scope}.effort={effort:?} 未建模");
        }
    }
    Ok(HarnessTuple {
        provider: file.provider,
        model: file.model,
        effort: file.effort,
        mode: file.mode,
    })
}

fn merge_tuple(base: &mut HarnessTuple, overlay: &HarnessTuple) {
    if overlay.provider.is_some() {
        base.provider.clone_from(&overlay.provider);
    }
    if overlay.model.is_some() {
        base.model.clone_from(&overlay.model);
    }
    if overlay.effort.is_some() {
        base.effort.clone_from(&overlay.effort);
    }
    if overlay.mode.is_some() {
        base.mode.clone_from(&overlay.mode);
    }
}

fn availability_for(entry: &HarnessEntry, action: Option<HarnessAction>) -> HarnessAvailability {
    if !entry.enabled {
        return HarnessAvailability::Unsupported("disabled by local config".into());
    }
    let driver = match HarnessId::parse(&entry.driver) {
        Ok(driver) => driver,
        Err(error) => return HarnessAvailability::Unknown(error),
    };
    if let Some(action) = action {
        if !driver_supports_action(driver, action) {
            return HarnessAvailability::Unsupported(format!(
                "driver {} does not support {}",
                driver.as_str(),
                action.as_str()
            ));
        }
        if let Some(reason) = unsupported_tuple_reason(driver, &entry.effective_tuple(action)) {
            return HarnessAvailability::Unsupported(reason);
        }
    } else {
        let mut any_supported = false;
        for candidate in [
            HarnessAction::Execute,
            HarnessAction::Review,
            HarnessAction::Consult,
        ] {
            if driver_supports_action(driver, candidate) {
                any_supported = true;
                if let Some(reason) =
                    unsupported_tuple_reason(driver, &entry.effective_tuple(candidate))
                {
                    return HarnessAvailability::Unsupported(reason);
                }
            }
        }
        if !any_supported {
            return HarnessAvailability::Unsupported(format!(
                "driver {} has no supported actions",
                driver.as_str()
            ));
        }
    }
    entry.executable_availability.clone()
}

fn driver_supports_action(driver: HarnessId, action: HarnessAction) -> bool {
    let action = match action {
        HarnessAction::Execute => DriverAction::Execute,
        HarnessAction::Review => DriverAction::Review,
        HarnessAction::Consult => DriverAction::Consult,
    };
    driver.supports_action(action)
}

fn unsupported_tuple_reason(driver: HarnessId, tuple: &HarnessTuple) -> Option<String> {
    if driver == HarnessId::Agy
        && tuple
            .effort()
            .is_some_and(|effort| !matches!(effort, "low" | "medium" | "high"))
    {
        return Some(format!(
            "driver agy does not support effort {:?}",
            tuple.effort().unwrap_or_default()
        ));
    }
    None
}

fn inspect_executable(path: &Path) -> HarnessAvailability {
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => HarnessAvailability::Unsupported(format!(
            "executable is not a regular file: {}",
            path.display()
        )),
        Ok(metadata) if metadata.permissions().mode() & 0o111 == 0 => {
            HarnessAvailability::Unsupported(format!(
                "executable is not executable: {}",
                path.display()
            ))
        }
        Ok(_) => HarnessAvailability::Supported,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            HarnessAvailability::Unsupported(format!("executable is missing: {}", path.display()))
        }
        Err(error) => HarnessAvailability::Unknown(format!(
            "cannot inspect executable {}: {error}",
            path.display()
        )),
    }
}

fn require_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("harness config parent 缺失: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "harness config parent 必须是 no-symlink regular directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn require_regular_no_symlink(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("harness config 缺失: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "harness config 必须是 no-symlink regular file: {}",
            path.display()
        );
    }
    Ok(metadata)
}

fn require_same_file(left: &Metadata, right: &Metadata, phase: &str) -> Result<()> {
    if left.dev() != right.dev()
        || left.ino() != right.ino()
        || left.len() != right.len()
        || left.mtime() != right.mtime()
        || left.mtime_nsec() != right.mtime_nsec()
        || left.ctime() != right.ctime()
        || left.ctime_nsec() != right.ctime_nsec()
    {
        bail!("harness config {phase} identity/bytes 漂移");
    }
    Ok(())
}

fn require_gitignored(main_root: &Path) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(main_root)
        .args(["check-ignore", "--quiet", "--", HARNESS_CONFIG_RELPATH])
        .status()
        .context("启动 git check-ignore 失败")?;
    match status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!("{} 必须由 gitignore 明确忽略", HARNESS_CONFIG_RELPATH),
        Some(code) => bail!("git check-ignore 异常退出 {code}"),
        None => bail!("git check-ignore 被信号终止"),
    }
}
