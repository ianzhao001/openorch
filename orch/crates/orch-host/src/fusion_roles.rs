//! Project-local role configuration. Native credentials and executable arguments
//! never enter this file; execution consumes a separate immutable run snapshot.
#![deny(missing_docs)]
use crate::channel::InvocationTuple;
/// Safe native configuration projection shared with the role editor.
pub use crate::native_discovery::parse_native_config;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MAX_BYTES: usize = 1024 * 1024;
/// A named consultation perspective, referencing a discovered harness identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionRole {
    /// Stable project-local identifier, independent of the display name.
    pub id: String,
    /// Human-readable name; never interpreted as markup or a command.
    pub name: String,
    /// Additional consultation instructions, preserving user text and newlines.
    pub instructions: String,
    /// Discovered installed identity or configured alias; not an executable path.
    pub harness: String,
    /// Explicit pins. Absent fields follow the freshly captured native defaults.
    pub fixed: InvocationTuple,
}
/// An ordered set of perspectives and its final synthesis role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionCombination {
    /// Stable identifier within this project.
    pub id: String,
    /// Human-readable combination name.
    pub name: String,
    /// Role IDs in display/input order, including temporarily disabled members.
    pub members: Vec<String>,
    /// Disabled member IDs; these must be present in members.
    pub disabled: Vec<String>,
    /// Synthesis role. None permits saving an incomplete draft.
    pub synthesizer: Option<String>,
}
/// Revisioned role library shared by linked worktrees of one Git repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionConfig {
    /// Optimistic concurrency revision assigned by the store.
    pub revision: u64,
    /// Reusable role definitions, capped at 32 entries.
    pub roles: Vec<FusionRole>,
    /// Saved combinations, capped at 16 entries; incomplete drafts are permitted.
    pub combinations: Vec<FusionCombination>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    config: FusionConfig,
}

/// Resolve only the four exposed fields; explicit values are never normalized.
pub fn resolve_tuple(native: &InvocationTuple, fixed: &InvocationTuple) -> InvocationTuple {
    InvocationTuple {
        provider: fixed.provider.clone().or_else(|| native.provider.clone()),
        model: fixed.model.clone().or_else(|| native.model.clone()),
        effort: fixed.effort.clone().or_else(|| native.effort.clone()),
        mode: fixed.mode.clone().or_else(|| native.mode.clone()),
    }
}
/// Validate identifiers, text bounds and role references without reading native settings.
pub fn validate_config(config: &FusionConfig) -> Result<()> {
    if config.roles.len() > 32 || config.combinations.len() > 16 {
        bail!("too_many_roles_or_combinations");
    }
    let mut roles = BTreeSet::new();
    for role in &config.roles {
        identifier(&role.id)?;
        label(&role.name)?;
        if !roles.insert(role.id.as_str()) {
            bail!("duplicate_role_id");
        }
        if role.instructions.len() > 16384 || role.instructions.contains('\0') {
            bail!("invalid_role_instructions");
        }
        if role.harness.is_empty()
            || role.harness.len() > 256
            || role.harness.trim() != role.harness
            || role.harness.chars().any(char::is_control)
        {
            bail!("invalid_harness_reference");
        }
        validate_tuple(&role.fixed)?;
    }
    let mut combinations = BTreeSet::new();
    for group in &config.combinations {
        identifier(&group.id)?;
        label(&group.name)?;
        if !combinations.insert(group.id.as_str()) {
            bail!("duplicate_combination_id");
        }
        let mut members = BTreeSet::new();
        for id in &group.members {
            if !roles.contains(id.as_str()) || !members.insert(id.as_str()) {
                bail!("invalid_member_reference");
            }
        }
        let mut disabled = BTreeSet::new();
        for id in &group.disabled {
            if !members.contains(id.as_str()) || !disabled.insert(id.as_str()) {
                bail!("invalid_disabled_reference");
            }
        }
        if group
            .synthesizer
            .as_ref()
            .is_some_and(|id| !roles.contains(id.as_str()))
        {
            bail!("invalid_synthesizer_reference");
        }
    }
    Ok(())
}
/// Reject ambiguous command values while preserving native opaque identifiers.
pub fn validate_tuple(tuple: &InvocationTuple) -> Result<()> {
    for value in [&tuple.provider, &tuple.model, &tuple.effort, &tuple.mode]
        .into_iter()
        .flatten()
    {
        if value.is_empty()
            || value.len() > 512
            || value.trim() != value
            || value.starts_with('-')
            || value.chars().any(char::is_control)
        {
            bail!("invalid_native_parameter");
        }
    }
    Ok(())
}
fn identifier(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("invalid_identifier");
    }
    Ok(())
}
fn label(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("invalid_name");
    }
    Ok(())
}
/// Resolve the canonical repository that owns shared local configuration.
pub fn project_root(root: &Path) -> Result<PathBuf> {
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        let output = std::process::Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(root)
            .output()?;
        if !output.status.success() {
            bail!("invalid_project_root");
        }
        Ok(output.stdout)
    };
    if run(&["rev-parse", "--is-bare-repository"])? == b"true\n" {
        bail!("bare_repository_not_supported");
    }
    let common = crate::gitx::canonical_worktree_common_dir(root)?;
    let list = run(&["worktree", "list", "--porcelain", "-z"])?;
    let first = list
        .split(|b| *b == 0)
        .next()
        .context("missing_primary_worktree")?;
    let name = first
        .strip_prefix(b"worktree ")
        .context("missing_primary_worktree")?;
    let main = fs::canonicalize(std::str::from_utf8(name).context("non_utf8_project_root")?)?;
    if crate::gitx::canonical_worktree_common_dir(&main)? != common {
        bail!("project_root_identity_changed");
    }
    Ok(main)
}

fn real_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() || !m.is_dir() {
        bail!("unsafe_config_directory");
    }
    Ok(())
}
#[cfg(target_os = "linux")]
const NO_FOLLOW: i32 = 0x20000;
#[cfg(not(target_os = "linux"))]
const NO_FOLLOW: i32 = 0x100;
#[cfg(target_os = "linux")]
const NONBLOCK: i32 = 0x800;
#[cfg(not(target_os = "linux"))]
const NONBLOCK: i32 = 0x4;
fn read_at(main: &Path) -> Result<FusionConfig> {
    let dir = main.join(".orch");
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FusionConfig::default()),
        Err(e) => return Err(e.into()),
        Ok(_) => real_dir(&dir)?,
    }
    let path = dir.join("fusion.json");
    // Nonblocking open lets us reject a FIFO before it can wait for a writer.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(NO_FOLLOW | NONBLOCK)
        .open(&path)
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FusionConfig::default()),
        other => other.context("unsafe_or_unreadable_config")?,
    };
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_BYTES as u64 {
        bail!("invalid_config_file");
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BYTES {
        bail!("config_too_large");
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).context("invalid_fusion_config")?;
    if envelope.version != 1 {
        bail!("unsupported_fusion_config_version");
    }
    validate_config(&envelope.config)?;
    Ok(envelope.config)
}
/// Read a consistent configuration; absence is an empty library and performs no writes.
pub fn load_config(root: &Path) -> Result<FusionConfig> {
    read_at(&project_root(root)?)
}
pub(crate) fn ensure_ignored(main: &Path) -> Result<()> {
    let mut complete = true;
    for path in [
        ".orch/fusion.json",
        ".orch/fusion.lock",
        ".orch/fusion-runs/probe",
        ".orch/native-discovery/probe",
    ] {
        let result = std::process::Command::new("git")
            .args(["check-ignore", "-q", path])
            .current_dir(main)
            .status()?;
        if result.code() == Some(1) {
            complete = false;
        } else if !result.success() {
            bail!("cannot_check_local_ignore");
        }
    }
    if complete {
        return Ok(());
    }
    let common = crate::gitx::canonical_worktree_common_dir(main)?;
    let info = common.join("info");
    if !info.exists() {
        fs::create_dir(&info)?;
    }
    real_dir(&info)?;
    let path = info.join("exclude");
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(NO_FOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.nlink() != 1 {
        bail!("unsafe_local_exclude");
    }
    file.write_all(b"\n# Local Fusion configuration and finite run artifacts\n/.orch/fusion.json\n/.orch/fusion.lock\n/.orch/fusion-runs/\n/.orch/native-discovery/\n")?;
    file.sync_all()?;
    Ok(())
}
/// Save under a project-local lock with revision compare-and-swap and atomic rename.
/// Global harness settings are untouched. A missing Git local ignore rule is appended.
pub fn save_config(
    root: &Path,
    expected_revision: u64,
    config: &FusionConfig,
) -> Result<FusionConfig> {
    validate_config(config)?;
    if config.revision != expected_revision {
        bail!("revision_conflict");
    }
    let main = project_root(root)?;
    let dir = main.join(".orch");
    if !dir.exists() {
        fs::create_dir(&dir)?;
    }
    real_dir(&dir)?;
    let lock_path = dir.join("fusion.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(NO_FOLLOW)
        .open(lock_path)?;
    if !lock_file.metadata()?.is_file() {
        bail!("unsafe_config_lock");
    }
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = lock.try_write().context("config_busy")?;
    let current = read_at(&main)?;
    if current.revision != expected_revision {
        bail!("revision_conflict");
    }
    let mut next = config.clone();
    next.revision = expected_revision
        .checked_add(1)
        .context("revision_overflow")?;
    let bytes = serde_json::to_vec_pretty(&Envelope {
        version: 1,
        config: next.clone(),
    })?;
    if bytes.len() > MAX_BYTES {
        bail!("config_too_large");
    }
    ensure_ignored(&main)?;
    let tmp = dir.join(format!(".fusion-{}.tmp", ulid::Ulid::new()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NO_FOLLOW)
        .open(&tmp)?;
    let identity = file.metadata()?;
    let result = (|| -> Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        real_dir(&dir)?;
        fs::rename(&tmp, dir.join("fusion.json"))?;
        File::open(&dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err()
        && fs::symlink_metadata(&tmp)
            .is_ok_and(|m| m.is_file() && m.ino() == identity.ino() && m.dev() == identity.dev())
    {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(next)
}
