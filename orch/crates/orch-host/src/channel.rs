//! Unified, immutable harness invocation preparation.
//!
//! Schema-3 callers capture local configuration and attachment bytes once, then
//! pass the resulting [`PreparedInvocation`] through render, preflight, spawn,
//! terminal observation, and receipt verification. This module deliberately
//! prepares without spawning; execution consumes only preflighted argv/cwd/env.
//! Neither stage consults ambient `PATH`, environment pins or legacy registries.

use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

use crate::harness::{DriverAction, DriverContract, HarnessId, HarnessTransport};
use crate::harness_config::{
    HarnessAction, HarnessConfigSnapshot, HarnessCwdPolicy, ResolvedHarness,
};

pub(crate) mod managed;
pub(crate) mod process;
/// Shared rich-message input and the hidden managed-supervisor entry.
pub use managed::{MessageInput, MessageSource, ResolvedMessage, resolve_message_input, run_wake_supervisor_from_stdin,
    run_direct_wake, is_direct_wake, direct_wake_status, cancel_direct_wake,
    WakeRunOutcome, ManagedWakeStatusView, ManagedWakeCancelDisposition, ManagedWakeCancelResult, AttachMode};

mod completion;
pub use completion::{classify_driver_failure, ChannelExitReason, DriverFailure};
pub(crate) mod smartclaw;

unsafe extern "C" {
    fn killpg(pgrp: i32, signal: i32) -> i32;
    fn getpgid(pid: i32) -> i32;
    fn setrlimit(resource: i32, limits: *const ChannelRlimit) -> i32;
}

#[repr(C)]
struct ChannelRlimit {
    current: u64,
    maximum: u64,
}

const CHANNEL_RLIMIT_FSIZE: i32 = 1;

/// Version of the unified schema-3 invocation-channel contract.
pub const UNIFIED_CHANNEL_CONTRACT_V1: u32 = 1;
const MAX_CHANNEL_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
static CHANNEL_CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Action requested from a configured harness driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvocationAction {
    /// Implement a task or perform another direct execution request.
    Execute,
    /// Inspect a fixed Git head and return a review verdict.
    Review,
    /// Answer an explicitly addressed consultation question.
    Consult,
}

impl InvocationAction {
    /// Return the stable action spelling used in digests and receipts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Review => "review",
            Self::Consult => "consult",
        }
    }

    fn config_action(self) -> HarnessAction {
        match self {
            Self::Execute => HarnessAction::Execute,
            Self::Review => HarnessAction::Review,
            Self::Consult => HarnessAction::Consult,
        }
    }

    fn driver_action(self) -> DriverAction {
        match self {
            Self::Execute => DriverAction::Execute,
            Self::Review => DriverAction::Review,
            Self::Consult => DriverAction::Consult,
        }
    }
}

/// Exact working-directory source selected by the captured configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CwdSelection {
    /// Use the canonical main-project root supplied by the caller.
    ProjectRoot,
    /// Use the exact task or review worktree supplied by the caller.
    TargetWorktree,
}

impl CwdSelection {
    /// Return the stable receipt spelling for this selection.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectRoot => "project-root",
            Self::TargetWorktree => "target-worktree",
        }
    }
}

/// Schema-3 dispatch route; a harness alias is a channel address, not an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchRouteV3 {
    /// Run in the planner-owned local worktree without a provider spawn.
    Local,
    /// Send execution through the named local harness configuration alias.
    Harness(String),
}

impl DispatchRouteV3 {
    /// Build a route from the CLI flags and enforce exact local-XOR-harness.
    pub fn from_flags(local: bool, harness: Option<&str>) -> Result<Self> {
        match (local, harness) {
            (true, None) => Ok(Self::Local),
            (false, Some(alias)) => {
                validate_alias(alias)?;
                Ok(Self::Harness(alias.to_owned()))
            }
            (false, None) => bail!("schema 3 dispatch 必须指定且只指定 --local 或 --harness"),
            (true, Some(_)) => bail!("schema 3 dispatch 的 --local 与 --harness 互斥"),
        }
    }
}

/// One attachment captured as immutable bytes for the lifetime of an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSnapshotV1 {
    path: PathBuf,
    bytes: Arc<[u8]>,
    sha256: String,
}

impl AttachmentSnapshotV1 {
    /// Absolute lexical path bound into the ordered manifest.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Exact bytes read through the identity-checked file handle.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// SHA-256 of the captured attachment bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Ordered, immutable attachment snapshots and their aggregate request digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentManifestV1 {
    entries: Arc<[AttachmentSnapshotV1]>,
    sha256: String,
}

impl AttachmentManifestV1 {
    /// Ordered attachment snapshots; order is part of the manifest identity.
    pub fn entries(&self) -> &[AttachmentSnapshotV1] {
        &self.entries
    }

    /// Domain-separated SHA-256 over path, order, length, byte hash, and bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Captured configured-path and resolved-target identity for one executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableIdentityV1 {
    configured_path: PathBuf,
    configured_identity: PathIdentity,
    resolved_path: PathBuf,
    target_identity: PathIdentity,
    sha256: String,
}

impl ExecutableIdentityV1 {
    /// Absolute spelling supplied by local configuration.
    pub fn configured_path(&self) -> &Path {
        &self.configured_path
    }

    /// Canonical executable target captured through the opened file handle.
    pub fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    /// Digest of both configured-link and resolved-target filesystem identities.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Capture ordered attachment bytes exactly once without following symlinks.
///
/// Each path must be absolute and lexically normalized. Every existing path
/// component is inspected with `lstat`; the leaf must be a regular file. File
/// identity is compared before open, through the open handle, after the read,
/// and after the read window so replacement or mutation is rejected.
pub fn capture_attachment_manifest_v1(paths: &[&Path]) -> Result<AttachmentManifestV1> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        validate_absolute_lexical_path(path, "attachment")?;
        let (before_chain, before) = require_regular_no_symlink_chain(path)?;
        let mut file = File::open(path)
            .with_context(|| format!("打开 attachment 失败: {}", path.display()))?;
        let opened_before = file
            .metadata()
            .with_context(|| format!("fstat attachment 失败: {}", path.display()))?;
        require_same_file(&before, &opened_before, "attachment open 前后")?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("读取 attachment 失败: {}", path.display()))?;
        let opened_after = file
            .metadata()
            .with_context(|| format!("读取后 fstat attachment 失败: {}", path.display()))?;
        let (after_chain, after) = require_regular_no_symlink_chain(path)?;
        require_same_file(&opened_before, &opened_after, "attachment 读取期间")?;
        require_same_file(&opened_after, &after, "attachment 读取完成后")?;
        if !same_path_topology(&before_chain, &after_chain) {
            bail!(
                "attachment 读取期间 parent/leaf topology 漂移: {}",
                path.display()
            );
        }
        if opened_after.len() != bytes.len() as u64 {
            bail!("attachment 读取期间长度漂移: {}", path.display());
        }

        entries.push(AttachmentSnapshotV1 {
            path: (*path).to_path_buf(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            bytes: Arc::from(bytes),
        });
    }

    let mut digest = Sha256::new();
    digest.update(b"orch-attachment-manifest-v1\0");
    update_len(&mut digest, entries.len());
    for (index, entry) in entries.iter().enumerate() {
        update_len(&mut digest, index);
        update_bytes(&mut digest, entry.path.as_os_str().as_bytes());
        update_bytes(&mut digest, entry.sha256.as_bytes());
        update_bytes(&mut digest, entry.bytes());
    }
    Ok(AttachmentManifestV1 {
        entries: Arc::from(entries),
        sha256: hex::encode(digest.finalize()),
    })
}

/// Caller-owned request data without raw argv, environment, or ability claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRequest {
    /// Local configuration alias selected for this action.
    pub alias: String,
    /// Requested invocation class.
    pub action: InvocationAction,
    /// Exact prompt bytes passed to the code-owned driver renderer.
    pub prompt: String,
    /// Canonical main-project root supplied by the live caller.
    pub project_root: PathBuf,
    /// Target task or review worktree, or an empty path when not applicable.
    pub target_worktree: PathBuf,
    /// Full lowercase fixed Git head, or an empty string when not applicable.
    pub target_head: String,
    /// Ordered attachment snapshot captured for this request.
    pub attachments: AttachmentManifestV1,
}

/// Provider/model/effort/mode facts recorded for an invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvocationTuple {
    /// Provider pin requested from the driver.
    pub provider: Option<String>,
    /// Model pin requested from the driver.
    pub model: Option<String>,
    /// Reasoning-effort pin requested from the driver.
    pub effort: Option<String>,
    /// Driver-owned action mode requested from the driver.
    pub mode: Option<String>,
}

/// Stable request facts retained separately from observed/effective facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedInvocation {
    /// Invocation action selected by the caller.
    pub action: InvocationAction,
    /// Tuple obtained from the immutable configuration snapshot.
    pub tuple: InvocationTuple,
}

impl RequestedInvocation {
    /// Requested provider pin, if one was configured.
    pub fn provider(&self) -> Option<&str> {
        self.tuple.provider.as_deref()
    }

    /// Requested model pin, if one was configured.
    pub fn model(&self) -> Option<&str> {
        self.tuple.model.as_deref()
    }

    /// Requested reasoning effort, if one was configured.
    pub fn effort(&self) -> Option<&str> {
        self.tuple.effort.as_deref()
    }

    /// Requested driver mode, if one was configured.
    pub fn mode(&self) -> Option<&str> {
        self.tuple.mode.as_deref()
    }
}

/// Fully prepared immutable action input shared by every later channel phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInvocation {
    alias: String,
    driver: HarnessId,
    executable: PathBuf,
    cwd: PathBuf,
    cwd_selection: CwdSelection,
    prompt: String,
    project_root: PathBuf,
    target_worktree: PathBuf,
    target_head: String,
    attachments: AttachmentManifestV1,
    config_digest: String,
    config_source: PathBuf,
    request_digest: String,
    requested: RequestedInvocation,
    effective: InvocationTuple,
    limits: crate::harness_config::HarnessInvocationLimits,
    driver_contract: DriverContract,
    executable_identity: ExecutableIdentityV1,
}

impl PreparedInvocation {
    /// Configuration alias that selected this driver profile.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Code-owned driver selected by the alias.
    pub fn driver(&self) -> HarnessId {
        self.driver
    }

    /// Absolute executable captured from local configuration.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// Exact cwd selected from project-root or target-worktree policy.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Source used to select [`Self::cwd`].
    pub fn cwd_selection(&self) -> CwdSelection {
        self.cwd_selection
    }

    /// Exact prompt bytes captured in the request.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Canonical main-project root captured by the request.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Target worktree captured for execution or review context.
    pub fn target_worktree(&self) -> &Path {
        &self.target_worktree
    }

    /// Full fixed Git head captured for execution or review context.
    pub fn target_head(&self) -> &str {
        &self.target_head
    }

    /// Immutable ordered attachment snapshots.
    pub fn attachments(&self) -> &AttachmentManifestV1 {
        &self.attachments
    }

    /// SHA-256 of the exact local configuration bytes used by this action.
    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    /// Absolute local configuration path bound to the captured snapshot.
    pub fn config_source(&self) -> &Path {
        &self.config_source
    }

    /// Domain-separated digest of caller request facts and captured attachments.
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }

    /// Requested action and tuple before any driver observation.
    pub fn requested(&self) -> &RequestedInvocation {
        &self.requested
    }

    /// Effective tuple used by the code-owned driver renderer.
    pub fn effective(&self) -> &InvocationTuple {
        &self.effective
    }

    /// Validated action limits from the same immutable configuration snapshot.
    pub fn limits(&self) -> &crate::harness_config::HarnessInvocationLimits {
        &self.limits
    }

    /// Code-owned transport, receipt, terminal, control, and observation facts.
    pub fn driver_contract(&self) -> DriverContract {
        self.driver_contract
    }

    /// Stable execution-level observation source owned by the driver.
    pub fn observation_source(&self) -> &'static str {
        self.driver_contract.observation_source
    }

    /// Executable identity that every live preflight must revalidate.
    pub fn executable_identity(&self) -> &ExecutableIdentityV1 {
        &self.executable_identity
    }

    /// Digest of the ordered attachment snapshot used by this action.
    pub fn attachment_manifest_digest(&self) -> &str {
        self.attachments.sha256()
    }
}

/// Runtime-owned identity needed to render an action without consulting legacy registries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationContextV1 {
    /// Durable action identity.
    pub action_id: String,
    /// Unique wake or consultation member identity.
    pub wake_id: String,
    /// Active collaboration round.
    pub round: String,
    /// Task identity, or a synthetic consultation task identity.
    pub task_id: String,
    /// Full task-shaped attempt identity.
    pub attempt_id: String,
    /// Absolute review output path; present only for review actions.
    pub review_output: Option<PathBuf>,
    /// Absolute current Orch executable exposed to managed wrappers.
    pub orch_executable: PathBuf,
    /// Positive wall-clock ceiling recorded for this invocation.
    pub deadline_secs: u64,
}

/// Code-owned final command rendered from one prepared invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedInvocationV1 {
    prepared: PreparedInvocation,
    context: InvocationContextV1,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    program_identity: ExecutableIdentityV1,
    wrapper_identity: Option<ExecutableIdentityV1>,
    command_digest: String,
}

impl RenderedInvocationV1 {
    /// Immutable prepared request consumed by this render.
    pub fn prepared(&self) -> &PreparedInvocation {
        &self.prepared
    }

    /// Runtime action identity bound into the render.
    pub fn context(&self) -> &InvocationContextV1 {
        &self.context
    }

    /// Absolute program followed by its exact code-owned arguments.
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// Complete controlled environment; spawn clears the ambient environment first.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Exact cwd already selected by captured local configuration.
    pub fn cwd(&self) -> &Path {
        self.prepared.cwd()
    }

    /// Digest of program identity, argv, controlled env, cwd, request, and context.
    pub fn command_digest(&self) -> &str {
        &self.command_digest
    }

    /// Identity digest of the exact program placed at argv[0].
    pub fn program_identity_digest(&self) -> &str {
        self.program_identity.sha256()
    }

    /// Optional code-owned wrapper identity retained for managed custody.
    pub(crate) fn wrapper_identity(&self) -> Option<&ExecutableIdentityV1> {
        self.wrapper_identity.as_ref()
    }
}

/// Render whose executable, cwd, repository, and HEAD were revalidated immediately before use.
#[derive(Debug)]
pub struct PreflightedInvocationV1 {
    rendered: RenderedInvocationV1,
}

impl PreflightedInvocationV1 {
    /// Revalidated command facts; no disk-backed configuration is reread.
    pub fn rendered(&self) -> &RenderedInvocationV1 {
        &self.rendered
    }

    /// Transfer the already-preflighted render into managed custody.
    pub(crate) fn into_rendered(self) -> RenderedInvocationV1 {
        self.rendered
    }
}

/// Recapture the executable identity digest used by supervisor admission.
pub fn executable_identity_digest_v1(path: &Path) -> Result<String> {
    Ok(capture_executable_identity(path)?.sha256)
}

/// Recapture a code-owned wrapper identity without requiring its own execute bit.
pub(crate) fn code_owned_wrapper_identity_digest_v1(path: &Path) -> Result<String> {
    Ok(capture_code_owned_wrapper_identity(path)?.sha256)
}

/// Render one driver-owned command without reading legacy registries, adapters, or presets.
///
/// Claude explicit mode `auto` uses `--permission-mode auto` without
/// `--dangerously-skip-permissions`; an unset mode preserves that legacy flag.
/// Other Claude modes and provider overrides remain rejected before execution.
///
/// Pi keeps tool execution in the selected worktree while receiving the original
/// project separately so its native project history can be opened there. CodeBuddy
/// likewise keeps native session persistence enabled instead of discarding history.
///
/// DSH preserves nonempty `DSH_HOME` for every selected action. Its wrapper pins
/// native model settings in a private per-invocation snapshot with watching disabled.
/// Only Consult replaces permission settings with readonly policy; other actions
/// preserve permission values and legacy preset plumbing without proving preset selection.
pub fn render_invocation_v1(
    prepared: PreparedInvocation,
    context: InvocationContextV1,
) -> Result<RenderedInvocationV1> {
    validate_context(&prepared, &context)?;
    let mut env = controlled_operational_environment(prepared.driver())?;
    let contract = prepared.driver_contract();
    let (program, wrapper_path, wrapper_identity) = match contract.wrapper {
        Some(wrapper) => {
            let wrapper = resolve_code_owned_wrapper(prepared.project_root(), wrapper)?;
            let identity = capture_code_owned_wrapper_identity(&wrapper)?;
            (PathBuf::from("/bin/sh"), Some(wrapper), Some(identity))
        }
        None => (
            prepared.executable_identity().resolved_path().to_path_buf(),
            None,
            None,
        ),
    };
    let program_identity = capture_executable_identity(&program)?;
    let mut argv = vec![path_text(&program, "rendered program")?];

    if let Some(wrapper) = wrapper_path {
        argv.push(path_text(&wrapper, "driver wrapper")?);
        argv.push(prepared.prompt().to_string());
        render_wrapper_environment(&prepared, &context, &mut env)?;
    } else {
        argv.extend(render_direct_arguments(&prepared, &context)?);
    }
    let command_digest = rendered_command_digest(
        &prepared,
        &context,
        &argv,
        &env,
        &program_identity,
        wrapper_identity.as_ref(),
    );
    Ok(RenderedInvocationV1 {
        prepared,
        context,
        argv,
        env,
        program_identity,
        wrapper_identity,
        command_digest,
    })
}

/// Revalidate every live filesystem/Git fact without rereading config or attachment bytes.
pub fn preflight_invocation_v1(rendered: RenderedInvocationV1) -> Result<PreflightedInvocationV1> {
    let prepared = rendered.prepared();
    if capture_executable_identity(prepared.executable())? != *prepared.executable_identity() {
        bail!("configured harness executable identity 漂移 before spawn");
    }
    let program = Path::new(
        rendered
            .argv()
            .first()
            .context("rendered invocation 缺 program")?,
    );
    if capture_executable_identity(program)? != rendered.program_identity {
        bail!("rendered program identity 漂移 before spawn");
    }
    if let Some(identity) = rendered.wrapper_identity.as_ref() {
        if capture_code_owned_wrapper_identity(identity.configured_path())? != *identity {
            bail!("code-owned wrapper identity 漂移 before spawn");
        }
    }

    require_canonical_exact_directory(prepared.project_root(), "project root")?;
    require_canonical_exact_directory(prepared.cwd(), "invocation cwd")?;
    let head_root = if prepared.target_worktree().as_os_str().is_empty() {
        prepared.project_root()
    } else {
        require_canonical_exact_directory(prepared.target_worktree(), "target worktree")?;
        if crate::gitx::canonical_worktree_common_dir(prepared.project_root())?
            != crate::gitx::canonical_worktree_common_dir(prepared.target_worktree())?
        {
            bail!("target worktree 属于不同 git common-dir");
        }
        prepared.target_worktree()
    };
    let actual_head = crate::gitx::rev_parse(head_root, "HEAD^{commit}")?;
    if actual_head != prepared.target_head() {
        bail!(
            "invocation fixed HEAD 漂移: expected={} actual={actual_head}",
            prepared.target_head()
        );
    }
    if prepared.cwd_selection() == CwdSelection::TargetWorktree
        && prepared.cwd() != prepared.target_worktree()
    {
        bail!("target-worktree cwd selection 未绑定 exact target worktree");
    }
    if prepared.cwd_selection() == CwdSelection::ProjectRoot
        && prepared.cwd() != prepared.project_root()
    {
        bail!("project-root cwd selection 未绑定 exact project root");
    }
    validate_dsh_runtime_target_v1(&rendered)?;
    Ok(PreflightedInvocationV1 { rendered })
}

fn validate_dsh_runtime_target_v1(rendered: &RenderedInvocationV1) -> Result<()> {
    if rendered.prepared.driver() != HarnessId::Dsh {
        return Ok(());
    }
    let orch = fs::canonicalize(&rendered.context.orch_executable)?;
    let runtime_target = orch
        .parent()
        .and_then(Path::parent)
        .context("DSH Orch binary 必须有两级 parent 以派生 runtime target")?;
    let profile = orch
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str());
    if !matches!(profile, Some("debug" | "release")) {
        bail!("DSH Orch binary 必须来自 Cargo debug/release target profile");
    }
    let cwd = fs::canonicalize(rendered.prepared.cwd())?;
    let manual =
        rendered.context.task_id == "MANUAL" && rendered.context.attempt_id == "MANUAL-A0000";
    let consult = rendered.prepared.requested().action == InvocationAction::Consult
        && rendered.context.task_id == "CONSULT"
        && rendered.context.attempt_id == "CONSULT-A0000";
    if !manual && !consult && runtime_target.starts_with(&cwd) {
        bail!(
            "DSH runtime target 必须位于 invocation cwd 之外；请从仓外、非临时 target 构建并运行 Orch: {}",
            runtime_target.display()
        );
    }
    let mut temporary_roots = vec![PathBuf::from("/tmp")];
    if let Some(tmpdir) = rendered.env.get("TMPDIR") {
        temporary_roots.push(PathBuf::from(tmpdir));
    }
    for temporary in temporary_roots {
        let Ok(temporary) = fs::canonicalize(temporary) else {
            continue;
        };
        if runtime_target.starts_with(&temporary) {
            bail!(
                "DSH runtime target 禁止位于 /tmp/TMPDIR；请从仓外非临时位置运行 Orch: {}",
                runtime_target.display()
            );
        }
    }
    let tag = runtime_target.join("CACHEDIR.TAG");
    let (before_chain, before) = require_regular_no_symlink_chain(&tag).with_context(|| {
        format!(
            "DSH runtime target 缺 Cargo CACHEDIR.TAG: {}",
            tag.display()
        )
    })?;
    let mut tag_file = File::open(&tag)
        .with_context(|| format!("打开 DSH Cargo CACHEDIR.TAG 失败: {}", tag.display()))?;
    let opened_before = tag_file.metadata()?;
    require_same_file(&before, &opened_before, "DSH Cargo CACHEDIR.TAG open 前后")?;
    let mut tag_bytes = Vec::new();
    (&mut tag_file)
        .take(4097)
        .read_to_end(&mut tag_bytes)
        .context("读取 DSH Cargo CACHEDIR.TAG 失败")?;
    if tag_bytes.len() > 4096 {
        bail!("DSH Cargo CACHEDIR.TAG 超过 4096-byte 上限");
    }
    let opened_after = tag_file.metadata()?;
    let (after_chain, after) = require_regular_no_symlink_chain(&tag)?;
    require_same_file(
        &opened_before,
        &opened_after,
        "DSH Cargo CACHEDIR.TAG 读取期间",
    )?;
    require_same_file(&opened_after, &after, "DSH Cargo CACHEDIR.TAG 读取完成后")?;
    if before_chain != after_chain || opened_after.len() != tag_bytes.len() as u64 {
        bail!("DSH Cargo CACHEDIR.TAG 读取期间 identity/length 漂移");
    }
    if !tag_bytes.starts_with(b"Signature: 8a477f597d28d172789f06886806bc55\n") {
        bail!("DSH runtime target CACHEDIR.TAG signature 非 canonical");
    }
    let probe = runtime_target.join(format!(
        ".orch-dsh-preflight-{}-{}",
        std::process::id(),
        CHANNEL_CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let probe_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
        .with_context(|| format!("DSH runtime target 不可写: {}", runtime_target.display()))?;
    drop(probe_file);
    fs::remove_file(&probe)
        .with_context(|| format!("清理 DSH runtime target probe 失败: {}", probe.display()))?;
    Ok(())
}

fn channel_capture_file(root: &Path, stream: &str) -> Result<(PathBuf, File)> {
    let scratch = root.join(".cowork-temp");
    let capture_dir = scratch.join("channel-capture");
    fs::create_dir_all(&capture_dir)?;
    for directory in [&scratch, &capture_dir] {
        let metadata = fs::symlink_metadata(directory)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("channel capture directory 必须是 real directory");
        }
    }
    let sequence = CHANNEL_CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = capture_dir.join(format!(
        "capture-{}-{sequence}-{stream}.bin",
        std::process::id()
    ));
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    Ok((path, file))
}

fn read_channel_capture(file: &File) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_CHANNEL_CAPTURE_BYTES {
        bail!("unified driver capture is not a bounded regular file");
    }
    // pread never moves the shared stdout/stderr offset while a child is live.
    let mut bytes = vec![0; metadata.len() as usize];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}

fn retain_channel_capture(root: &Path, path: PathBuf, file: &File, bytes: &[u8], stream: &str) -> Result<PathBuf> {
    let opened = file.metadata()?;
    if fs::symlink_metadata(&path).is_ok_and(|current|
        current.is_file() && !current.file_type().is_symlink() && current.nlink() == 1
        && current.dev() == opened.dev() && current.ino() == opened.ino()) {
        return Ok(path);
    }
    // A child may replace the pathname with a FIFO/symlink. Preserve the bytes
    // from the original handle in a new owned file, without following or
    // deleting that replacement. An unclosed invocation still has only a prefix.
    let (recovered, mut target) = channel_capture_file(root, stream)?;
    target.write_all(bytes)?;
    target.sync_all()?;
    Ok(recovered)
}

fn channel_process_group_empty(pgid: i32) -> Result<bool> {
    // SAFETY: signal zero observes a group; it does not cancel any process.
    if unsafe { killpg(pgid, 0) } == 0 { return Ok(false); }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(3) { return Ok(true); } // ESRCH on supported Unix hosts.
    Err(error).context("cannot establish invocation process-group termination")
}

fn await_channel_process_group_empty(pgid: i32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if channel_process_group_empty(pgid)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("unified driver process group 未在 bounded cleanup 内收敛");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Actual synchronous execution facts. Process-group closure is not proof that
/// a persistent native backend has ended; its driver must supply that evidence.
#[derive(Debug)]
pub struct ChannelExecution {
    /// PID created by this invocation; a later PID observation still needs identity checks.
    pub process_id: u32,
    /// Actual reaped OS status, absent when it could not be observed.
    pub status: Option<ExitStatus>,
    /// Captured stdout bytes; an incomplete observation may only provide a prefix.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes, separate from the bounded redacted diagnostic.
    pub stderr: Vec<u8>,
    /// Runtime stop reason; never inferred from exit code or elapsed duration.
    pub reason: ChannelExitReason,
    /// The effective hard deadline selected from the immutable invocation snapshot.
    pub hard_deadline_secs: u64,
    /// Monotonic offset of the first complete stdout line or JSON object observed.
    /// This is activity evidence only, not a trusted terminal or proof of progress.
    pub first_frame_after_millis: Option<u64>,
    /// Monotonic offset at which the leader's actual OS exit status was reaped.
    pub leader_exited_after_millis: Option<u64>,
    /// Monotonic duration through final capture, including any owned cleanup.
    pub elapsed_millis: u64,
    /// Whether the owned process group was observed absent without an observation error.
    pub process_group_terminated: bool,
    /// Redacted observation errors; none authorizes cancellation by itself.
    pub observation_errors: Vec<String>,
    /// Private raw capture (or recovered prefix), absent if preservation failed.
    /// It can still grow while the invocation is unclosed.
    pub stdout_capture_path: Option<PathBuf>,
    /// Private stderr capture, with the same absence/closure qualification as stdout.
    pub stderr_capture_path: Option<PathBuf>,
    pub(crate) native_final: Option<serde_json::Value>,
}

impl ChannelExecution {
    /// SmartClaw's separately verified native final, never cut from raw dialogue.
    pub fn native_final_text(&self) -> Option<&str> {
        let native = self.native_final.as_ref()?;
        if native["projectionStatus"] != "available" || native["nativeTerminated"] != true { return None; }
        native.pointer("/final/text")?.as_str()
    }
    /// Actual numeric exit code; signals and missing status both have no numeric code.
    pub fn exit_code(&self) -> Option<i32> { self.status.and_then(|status| status.code()) }

    /// Whether an actual observed exit status is successful; missing status is false.
    pub fn success(&self) -> bool { self.status.is_some_and(|status| status.success()) }

    /// Compact metadata that does not copy the raw streams into a manifest.
    pub fn facts(&self) -> serde_json::Value {
        serde_json::json!({
            "processId": self.process_id, "processGroupId": self.process_id,
            "exitReason": self.reason, "exitCode": self.exit_code(),
            "hardDeadlineSecs": self.hard_deadline_secs,
            "firstFrameAfterMillis": self.first_frame_after_millis,
            "leaderExitedAfterMillis": self.leader_exited_after_millis,
            "elapsedMillis": self.elapsed_millis,
            "processGroupTerminated": self.process_group_terminated,
            // Killing/reaping this client is not a provider-owned cancellation receipt.
            "upstreamTermination": "unconfirmed",
            "rawCaptureStable": self.process_group_terminated && self.status.is_some()
                && self.observation_errors.is_empty() && self.stdout_capture_path.is_some()
                && self.stderr_capture_path.is_some(),
            "observationErrors": self.observation_errors,
            "stdoutCapturePath": self.stdout_capture_path,
            "stderrCapturePath": self.stderr_capture_path,
            "nativeFinal": self.native_final.as_ref().map(smartclaw::without_text),
        })
    }
}

#[derive(Default)]
struct FirstFrameObservation {
    offset: u64,
    pending: Vec<u8>,
    at_millis: Option<u64>,
}

impl FirstFrameObservation {
    fn observe(&mut self, file: &File, started: Instant) -> Result<()> {
        if self.at_millis.is_some() { return Ok(()); }
        let length = file.metadata()?.len();
        if length > MAX_CHANNEL_CAPTURE_BYTES { bail!("stdout observation exceeds capture bound"); }
        while self.offset < length {
            let mut chunk = [0u8; 8192];
            let wanted = (length - self.offset).min(chunk.len() as u64) as usize;
            let read = file.read_at(&mut chunk[..wanted], self.offset)?;
            if read == 0 { break; }
            self.offset += read as u64;
            self.pending.extend_from_slice(&chunk[..read]);
            if self.pending.contains(&b'\n') || serde_json::from_slice::<serde_json::Value>(&self.pending)
                .is_ok_and(|value| value.is_object()) {
                self.at_millis = Some(started.elapsed().as_millis() as u64);
                self.pending.clear();
                break;
            }
        }
        Ok(())
    }
}

/// Spawn a preflighted command with a closed environment and capture its output.
///
/// This synchronous primitive is used by consultation and tests. Managed wake
/// custody consumes the same preflighted argv/env/cwd through its supervisor.
pub fn run_preflighted_invocation_v1(invocation: PreflightedInvocationV1) -> Result<ChannelExecution> {
    let rendered = invocation.rendered;
    let (program, args) = rendered
        .argv
        .split_first()
        .context("preflighted invocation 缺 program")?;
    let (stdout_path, stdout_file) =
        channel_capture_file(rendered.prepared.project_root(), "stdout")?;
    let (stderr_path, stderr_file) =
        match channel_capture_file(rendered.prepared.project_root(), "stderr") {
            Ok(capture) => capture,
            Err(error) => {
                let _ = fs::remove_file(&stdout_path);
                return Err(error);
            }
        };
    let child_stdout = match stdout_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Err(error).context("clone unified stdout capture failed");
        }
    };
    let child_stderr = match stderr_file.try_clone() {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Err(error).context("clone unified stderr capture failed");
        }
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(rendered.prepared.cwd())
        .env_clear()
        .envs(&rendered.env)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr));
    // SAFETY: the closure calls only async-signal-safe `setrlimit` with a
    // stack-owned POD value before exec; it performs no allocation or locking.
    unsafe {
        command.pre_exec(|| {
            let limits = ChannelRlimit {
                current: MAX_CHANNEL_CAPTURE_BYTES,
                maximum: MAX_CHANNEL_CAPTURE_BYTES,
            };
            if setrlimit(CHANNEL_RLIMIT_FSIZE, &limits) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let child = command.spawn().with_context(|| {
        format!(
            "spawn unified harness driver 失败: alias={} driver={}",
            rendered.prepared.alias(),
            rendered.prepared.driver().as_str()
        )
    });
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);
            return Err(error);
        }
    };
    let deadline = Duration::from_secs(rendered.context.deadline_secs);
    let pgid = child.id() as i32;
    let mut status = None;
    let mut leader_exited = None;
    let mut frame = FirstFrameObservation::default();
    let mut errors = Vec::new();
    let mut group_terminated = false;
    let mut reason = ChannelExitReason::Exited;
    loop {
        if let Err(error) = frame.observe(&stdout_file, started) {
            errors.push(format!("stdout observation failed: {error:#}"));
            reason = ChannelExitReason::ObservationFailed;
            break; // observation failure alone never signals a process.
        }
        if status.is_some() {
            match channel_process_group_empty(pgid) {
                Ok(true) => { group_terminated = true; break; }
                Ok(false) => {}
                Err(error) => {
                    errors.push(format!("{error:#}"));
                    reason = ChannelExitReason::ObservationFailed;
                    break;
                }
            }
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            // Drain an already-finished child before choosing a deadline winner.
            // A delayed observation is not proof that the runtime killed it.
            if status.is_none() {
                match child.try_wait() {
                    Ok(Some(actual)) => {
                        status = Some(actual);
                        leader_exited = Some(started.elapsed().as_millis() as u64);
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(format!("deadline status observation failed: {error}"));
                        reason = ChannelExitReason::ObservationFailed;
                        break;
                    }
                }
            }
            reason = ChannelExitReason::HardDeadline;
            // A live, unreaped leader keeps its PID ownership. Once reaped, do
            // not signal a potentially reused group solely by its old number.
            if status.is_none() && unsafe { getpgid(child.id() as i32) } == pgid {
                // SAFETY: unreaped owned child is still the group leader.
                if unsafe { killpg(pgid, 9) } == 0 {
                    // The unreaped child remains owned even if it changes group
                    // between getpgid and killpg. Keep final reap bounded too.
                    if let Err(error) = child.kill() {
                        errors.push(format!("direct owned-child cancellation: {error}"));
                    }
                    match child.wait_timeout(Duration::from_secs(2)) {
                        Ok(Some(actual)) => { status = Some(actual); leader_exited = Some(started.elapsed().as_millis() as u64); }
                        Ok(None) => errors.push("owned child remains unclosed after deadline cleanup; HOLD".into()),
                        Err(error) => errors.push(format!("reap after hard deadline failed: {error}")),
                    }
                    match await_channel_process_group_empty(pgid) {
                        Ok(()) => group_terminated = true,
                        Err(error) => errors.push(format!("{error:#}")),
                    }
                } else {
                    errors.push(format!("owned deadline cancellation failed: {}", std::io::Error::last_os_error()));
                }
            } else {
                errors.push("hard deadline reached without current leader ownership proof; HOLD".into());
            }
            break;
        }
        if status.is_none() {
            match child.wait_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(Some(actual)) => { status = Some(actual); leader_exited = Some(started.elapsed().as_millis() as u64); }
                Ok(None) => {}
                Err(error) => {
                    errors.push(format!("child observation failed: {error}"));
                    reason = ChannelExitReason::ObservationFailed;
                    break;
                }
            }
        } else {
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }
    let stdout = read_channel_capture(&stdout_file).unwrap_or_else(|error| {
        errors.push(format!("stdout final capture failed: {error:#}")); Vec::new()
    });
    let stderr = read_channel_capture(&stderr_file).unwrap_or_else(|error| {
        errors.push(format!("stderr final capture failed: {error:#}")); Vec::new()
    });
    let stdout_capture_path = retain_channel_capture(rendered.prepared.project_root(), stdout_path,
        &stdout_file, &stdout, "stdout-recovered").map_err(|error| {
            errors.push(format!("stdout preservation failed: {error:#}"));
        }).ok();
    let stderr_capture_path = retain_channel_capture(rendered.prepared.project_root(), stderr_path,
        &stderr_file, &stderr, "stderr-recovered").map_err(|error| {
            errors.push(format!("stderr preservation failed: {error:#}"));
        }).ok();
    if !errors.is_empty() && reason == ChannelExitReason::Exited { reason = ChannelExitReason::ObservationFailed; }
    let native_final = if rendered.prepared.driver() == HarnessId::SmartClaw {
        let session = format!("orch-wake-{}", rendered.context.wake_id);
        let prompt_sha = hex::encode(Sha256::digest(rendered.prepared.prompt().as_bytes()));
        let database = smartclaw::database_from_environment(&rendered.env);
        Some(match database {
            Some(database) => smartclaw::inspect_native_final(rendered.prepared.project_root(),
                &database, &session, rendered.prepared.cwd(), &prompt_sha, &stdout, status.is_some() && group_terminated),
            None => Err(anyhow::anyhow!("captured HOME is unavailable for native observation")),
        }.unwrap_or_else(|error| smartclaw::unavailable(&format!("{error:#}"))))
    } else { None };
    Ok(ChannelExecution {
        process_id: child.id(), status, stdout, stderr, reason,
        hard_deadline_secs: rendered.context.deadline_secs,
        first_frame_after_millis: frame.at_millis,
        leader_exited_after_millis: leader_exited,
        elapsed_millis: started.elapsed().as_millis() as u64,
        process_group_terminated: group_terminated,
        observation_errors: errors.into_iter().map(|error| crate::redact::redact_full(&error)).collect(),
        stdout_capture_path,
        stderr_capture_path,
        native_final,
    })
}

fn validate_context(prepared: &PreparedInvocation, context: &InvocationContextV1) -> Result<()> {
    for (label, value) in [
        ("actionId", context.action_id.as_str()),
        ("wakeId", context.wake_id.as_str()),
        ("round", context.round.as_str()),
        ("taskId", context.task_id.as_str()),
        ("attemptId", context.attempt_id.as_str()),
    ] {
        if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
            bail!("invocation context {label} 必须是 non-blank exact string");
        }
    }
    let prefix = format!("{}-A", context.task_id);
    let suffix = context.attempt_id.strip_prefix(&prefix);
    if suffix
        .is_none_or(|value| value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_digit()))
    {
        bail!("invocation context attemptId 必须是完整 task-A0000 identity");
    }
    if context.deadline_secs == 0 {
        bail!("invocation deadlineSecs 必须为正数");
    }
    validate_absolute_lexical_path(&context.orch_executable, "orch executable")?;
    capture_executable_identity(&context.orch_executable)?;
    match prepared.requested().action {
        InvocationAction::Review => {
            let output = context
                .review_output
                .as_deref()
                .context("review invocation 必须给出 review output path")?;
            validate_absolute_lexical_path(output, "review output")?;
        }
        InvocationAction::Execute | InvocationAction::Consult => {
            if context.review_output.is_some() {
                bail!("non-review invocation 禁止 review output path");
            }
        }
    }
    Ok(())
}

fn controlled_operational_environment(driver: HarnessId) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::from([(
        "PATH".to_string(),
        "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin".to_string(),
    )]);
    for key in ["HOME", "TMPDIR", "LANG", "LC_ALL", "TERM"] {
        let Some(value) = std::env::var_os(key) else {
            continue;
        };
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("operational env {key} 不是 UTF-8"))?;
        if !value.is_empty() {
            env.insert(key.to_string(), value);
        }
    }
    // Claude Code's macOS subscription lookup keys its persisted login by the
    // non-secret account name. HOME alone reports `loggedIn=false` after
    // env_clear, so expose USER only to this driver instead of widening every
    // harness environment.
    if driver == HarnessId::Claude {
        if let Some(value) = std::env::var_os("USER") {
            let value = value
                .into_string()
                .map_err(|_| anyhow::anyhow!("operational env USER 不是 UTF-8"))?;
            if !value.is_empty() {
                env.insert("USER".to_string(), value);
            }
        }
    }
    Ok(env)
}

fn resolve_code_owned_wrapper(root: &Path, wrapper: &str) -> Result<PathBuf> {
    let relative = Path::new(wrapper);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("driver wrapper 必须是 canonical repository-relative path");
    }
    if !crate::has_selfhost_state(root)? {
        // A standalone target is not the installation of orch. Never execute
        // a same-named script supplied by that target repository. Portable
        // distributions may place the unchanged wrapper beside the binary;
        // source builds use their own compile-time checkout.
        let expected: &[u8] = match wrapper {
            "coordination/scripts/wake-multica.sh" => include_bytes!("../../../../coordination/scripts/wake-multica.sh"),
            "orch/scripts/wake-dsh-stream.sh" => include_bytes!("../../../scripts/wake-dsh-stream.sh"),
            "orch/scripts/wake-pi-stream.sh" => include_bytes!("../../../scripts/wake-pi-stream.sh"),
            "orch/scripts/wake-zcode-stream.sh" => include_bytes!("../../../scripts/wake-zcode-stream.sh"),
            _ => bail!("unknown code-owned wrapper asset {wrapper}"),
        };
        let source_root = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")).ok();
        let executable = std::env::current_exe()?;
        let mut candidates = source_root.into_iter().map(|root| root.join(relative)).collect::<Vec<_>>();
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join("scripts").join(relative.file_name().context("wrapper basename missing")?));
        }
        for candidate in candidates {
            if fs::symlink_metadata(&candidate).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) { continue; }
            require_regular_no_symlink_chain(&candidate)?;
            if fs::read(&candidate)? == expected { return Ok(fs::canonicalize(candidate)?); }
        }
        bail!("installed code-owned wrapper is missing or differs from this build: {wrapper}");
    }
    let candidate = root.join(relative);
    require_regular_no_symlink_chain(&candidate)?;
    let canonical_root = fs::canonicalize(root)?;
    let canonical_candidate = fs::canonicalize(&candidate)?;
    if !canonical_candidate.starts_with(&canonical_root) {
        bail!("driver wrapper 逃出 project root");
    }
    Ok(candidate)
}

fn path_text(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{label} 不是 UTF-8 path: {}", path.display()))
}

fn render_wrapper_environment(
    prepared: &PreparedInvocation,
    context: &InvocationContextV1,
    env: &mut BTreeMap<String, String>,
) -> Result<()> {
    let role = match prepared.requested().action {
        InvocationAction::Execute => "implement",
        InvocationAction::Review => "review",
        InvocationAction::Consult => "consult",
    };
    match prepared.driver() {
        HarnessId::Pi | HarnessId::ZCode | HarnessId::Dsh => {
            let provider = required_tuple(prepared.effective().provider.as_deref(), "provider")?;
            let model = required_tuple(prepared.effective().model.as_deref(), "model")?;
            let effort = required_tuple(prepared.effective().effort.as_deref(), "effort")?;
            let review_output = context
                .review_output
                .as_deref()
                .map(|path| path_text(path, "review output"))
                .transpose()?
                .unwrap_or_else(|| crate::harness::NO_REVIEW_OUTPUT.to_string());
            env.extend(BTreeMap::from([
                (
                    "ORCH_HARNESS_ID".to_string(),
                    prepared.driver().as_str().to_string(),
                ),
                (
                    "ORCH_HARNESS_ACTION_ID".to_string(),
                    context.action_id.clone(),
                ),
                ("ORCH_HARNESS_WAKE_ID".to_string(), context.wake_id.clone()),
                ("ORCH_HARNESS_ROUND".to_string(), context.round.clone()),
                ("ORCH_HARNESS_TASK_ID".to_string(), context.task_id.clone()),
                (
                    "ORCH_HARNESS_ATTEMPT_ID".to_string(),
                    context.attempt_id.clone(),
                ),
                ("ORCH_HARNESS_ROLE".to_string(), role.to_string()),
                (
                    "ORCH_HARNESS_CWD".to_string(),
                    path_text(prepared.cwd(), "cwd")?,
                ),
                (
                    "ORCH_HARNESS_FIXED_HEAD".to_string(),
                    prepared.target_head().to_string(),
                ),
                ("ORCH_HARNESS_PROVIDER".to_string(), provider.to_string()),
                ("ORCH_HARNESS_MODEL".to_string(), model.to_string()),
                ("ORCH_HARNESS_EFFORT".to_string(), effort.to_string()),
                (
                    "ORCH_HARNESS_PROVIDER_BIN".to_string(),
                    path_text(prepared.executable(), "provider executable")?,
                ),
                ("ORCH_HARNESS_REVIEW_OUTPUT_PATH".to_string(), review_output),
                (
                    "ORCH_HARNESS_ORCH_BIN".to_string(),
                    path_text(&context.orch_executable, "orch executable")?,
                ),
                (
                    "ORCH_HARNESS_DEADLINE_SECS".to_string(),
                    context.deadline_secs.to_string(),
                ),
            ]));
            if prepared.driver() == HarnessId::Pi {
                env.insert(
                    "ORCH_PI_PROJECT_ROOT".to_string(),
                    path_text(prepared.project_root(), "Pi project root")?,
                );
            } else if prepared.driver() == HarnessId::Dsh {
                // All selected DSH actions snapshot the same native account settings.
                if let Some(value) = std::env::var_os("DSH_HOME") {
                    let value = value.to_str().context("DSH_HOME is not UTF-8")?;
                    if !value.trim().is_empty() {
                        env.insert("DSH_HOME".to_string(), value.to_string());
                    }
                }
                match prepared.effective().mode.as_deref() {
                    Some("minimal") => {
                        env.insert("ORCH_DSH_PRESET".to_string(), "minimal".to_string());
                    }
                    Some("headless") => {
                        env.insert("ORCH_DSH_PROFILE".to_string(), "headless".to_string());
                    }
                    Some(other) => bail!("dsh mode {other:?} 未建模"),
                    None => {}
                }
            } else if prepared.effective().mode.is_some() {
                bail!("managed driver {} 不支持 mode", prepared.driver().as_str());
            }
        }
        HarnessId::SmartClaw => {
            require_empty_tuple(prepared)?;
            env.insert(
                "ORCH_MULTICA_SESSION".to_string(),
                format!("orch-wake-{}", context.wake_id),
            );
            env.insert(
                "ORCH_MULTICA_CWD".to_string(),
                path_text(prepared.cwd(), "SmartClaw cwd")?,
            );
            env.insert(
                "ORCH_MULTICA_TIMEOUT".to_string(),
                context.deadline_secs.to_string(),
            );
        }
        HarnessId::Dclaw => require_empty_tuple(prepared)?,
        other => bail!("driver {} 的 wrapper render 未建模", other.as_str()),
    }
    Ok(())
}

fn render_direct_arguments(prepared: &PreparedInvocation, context: &InvocationContextV1) -> Result<Vec<String>> {
    let tuple = prepared.effective();
    let prompt = prepared.prompt().to_string();
    let cwd = path_text(prepared.cwd(), "invocation cwd")?;
    match prepared.driver() {
        HarnessId::OpenCode => {
            let mut args = vec!["run".to_string(), prompt];
            if let Some(model) = joined_provider_model(tuple) {
                args.extend(["--model".to_string(), model]);
            }
            if let Some(effort) = tuple.effort.as_deref() {
                args.extend(["--variant".to_string(), effort.to_string()]);
            }
            args.extend(["--format".to_string(), "json".to_string()]);
            match tuple.mode.as_deref() {
                Some("pure") => args.push("--pure".to_string()),
                Some(other) => bail!("opencode mode {other:?} 未建模"),
                None => {}
            }
            args.extend(["--dir".to_string(), cwd]);
            Ok(args)
        }
        HarnessId::Codex => {
            if tuple.provider.is_some() {
                bail!("codex provider pin 未建模；不得静默忽略");
            }
            let mut args = vec![
                "exec".to_string(),
                prompt,
                "--json".to_string(),
                "--skip-git-repo-check".to_string(),
            ];
            if let Some(model) = tuple.model.as_deref() {
                args.extend(["-m".to_string(), model.to_string()]);
            }
            if let Some(effort) = tuple.effort.as_deref() {
                args.extend([
                    "-c".to_string(),
                    format!("model_reasoning_effort=\"{effort}\""),
                ]);
            }
            match tuple.mode.as_deref() {
                Some("fast_mode") => {
                    args.extend(["--enable".to_string(), "fast_mode".to_string()]);
                }
                Some(other) => bail!("codex mode {other:?} 未建模"),
                None => {}
            }
            args.extend([
                "-c".to_string(),
                format!(
                    "sandbox_workspace_write.writable_roots=[\"{}/.git\"]",
                    prepared.project_root().display()
                ),
            ]);
            Ok(args)
        }
        HarnessId::Claude => {
            if tuple.provider.is_some()
                || !matches!(tuple.mode.as_deref(), None | Some("auto"))
            {
                bail!("claude provider/mode pin 未建模；不得静默忽略");
            }
            let mut args = Vec::new();
            if let Some(model) = tuple.model.as_deref() {
                args.extend(["--model".to_string(), model.to_string()]);
            }
            if let Some(effort) = tuple.effort.as_deref() {
                args.extend(["--effort".to_string(), effort.to_string()]);
            }
            args.extend([
                "-p".to_string(),
                prompt,
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ]);
            if tuple.mode.as_deref() == Some("auto") {
                args.extend(["--permission-mode".to_string(), "auto".to_string()]);
            } else {
                args.push("--dangerously-skip-permissions".to_string());
            }
            Ok(args)
        }
        HarnessId::Cursor => {
            require_empty_tuple(prepared)?;
            Ok(vec![
                "-p".to_string(),
                prompt,
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--force".to_string(),
            ])
        }
        HarnessId::Mimo => {
            if tuple.provider.is_some() {
                bail!("mimo provider pin 未建模；不得静默忽略");
            }
            let mut args = vec!["run".to_string(), prompt];
            if let Some(model) = tuple.model.as_deref() {
                args.extend(["--model".to_string(), model.to_string()]);
            }
            if let Some(effort) = tuple.effort.as_deref() {
                args.extend(["--variant".to_string(), effort.to_string()]);
            }
            args.extend(["--format".to_string(), "json".to_string()]);
            match tuple.mode.as_deref() {
                Some("pure") => args.push("--pure".to_string()),
                Some(other) => bail!("mimo mode {other:?} 未建模"),
                None => {}
            }
            args.extend([
                "--dangerously-skip-permissions".to_string(),
                "--dir".to_string(),
                cwd,
            ]);
            Ok(args)
        }
        HarnessId::CodeBuddy => {
            if tuple.provider.is_some() || tuple.mode.is_some() {
                bail!("codebuddy provider/mode pin 未建模；不得静默忽略");
            }
            let mut args = vec!["-p".to_string(), prompt];
            if let Some(model) = tuple.model.as_deref() {
                args.extend(["--model".to_string(), model.to_string()]);
            }
            if let Some(effort) = tuple.effort.as_deref() {
                args.extend(["--effort".to_string(), effort.to_string()]);
            }
            args.extend([
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]);
            Ok(args)
        }
        HarnessId::Agy => {
            if tuple
                .provider
                .as_deref()
                .is_some_and(|value| value != "antigravity")
                || tuple.mode.is_some()
            {
                bail!("agy provider/mode pin 未建模；不得静默忽略");
            }
            // The CLI has its own print wait. Carry the already-selected
            // deadline instead of silently retaining a shorter client default.
            let mut args = vec!["-p".to_string(), prompt, "--print-timeout".to_string(),
                format!("{}s", context.deadline_secs)];
            if let Some(model) = tuple.model.as_deref() {
                args.extend(["--model".to_string(), model.to_string()]);
            }
            if let Some(effort) = tuple.effort.as_deref() {
                args.extend(["--effort".to_string(), effort.to_string()]);
            }
            args.push("--dangerously-skip-permissions".to_string());
            Ok(args)
        }
        other => bail!("driver {} 不是 direct CLI render", other.as_str()),
    }
}

fn required_tuple<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str> {
    value.with_context(|| format!("managed driver 缺 required {field} pin"))
}

fn require_empty_tuple(prepared: &PreparedInvocation) -> Result<()> {
    let tuple = prepared.effective();
    if tuple.provider.is_some()
        || tuple.model.is_some()
        || tuple.effort.is_some()
        || tuple.mode.is_some()
    {
        bail!(
            "driver {} 不接受 provider/model/effort/mode pin",
            prepared.driver().as_str()
        );
    }
    Ok(())
}

fn joined_provider_model(tuple: &InvocationTuple) -> Option<String> {
    match (tuple.provider.as_deref(), tuple.model.as_deref()) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (None, Some(model)) => Some(model.to_string()),
        (Some(provider), None) => Some(provider.to_string()),
        (None, None) => None,
    }
}

fn rendered_command_digest(
    prepared: &PreparedInvocation,
    context: &InvocationContextV1,
    argv: &[String],
    env: &BTreeMap<String, String>,
    program_identity: &ExecutableIdentityV1,
    wrapper_identity: Option<&ExecutableIdentityV1>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"orch-rendered-invocation-v1\0");
    update_bytes(&mut digest, prepared.request_digest().as_bytes());
    update_bytes(&mut digest, prepared.config_digest().as_bytes());
    update_bytes(
        &mut digest,
        prepared.attachment_manifest_digest().as_bytes(),
    );
    update_bytes(&mut digest, program_identity.sha256().as_bytes());
    update_optional(
        &mut digest,
        wrapper_identity.map(ExecutableIdentityV1::sha256),
    );
    update_len(&mut digest, argv.len());
    for value in argv {
        update_bytes(&mut digest, value.as_bytes());
    }
    update_len(&mut digest, env.len());
    for (key, value) in env {
        update_bytes(&mut digest, key.as_bytes());
        update_bytes(&mut digest, value.as_bytes());
    }
    update_bytes(&mut digest, prepared.cwd().as_os_str().as_bytes());
    for value in [
        context.action_id.as_str(),
        context.wake_id.as_str(),
        context.round.as_str(),
        context.task_id.as_str(),
        context.attempt_id.as_str(),
    ] {
        update_bytes(&mut digest, value.as_bytes());
    }
    update_optional(
        &mut digest,
        context.review_output.as_deref().and_then(Path::to_str),
    );
    update_bytes(&mut digest, context.orch_executable.as_os_str().as_bytes());
    digest.update(context.deadline_secs.to_be_bytes());
    hex::encode(digest.finalize())
}

fn require_canonical_exact_directory(path: &Path, label: &str) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("{label} 缺失: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} 必须是 regular no-symlink directory");
    }
    let canonical = fs::canonicalize(path)?;
    if canonical != path {
        bail!(
            "{label} 必须使用 exact canonical path: expected={} actual={}",
            path.display(),
            canonical.display()
        );
    }
    Ok(())
}

/// Resolve a request entirely from one immutable configuration snapshot.
///
/// This is a lexical preparation seam: live callers separately prove canonical
/// repository/worktree/HEAD and executable identity immediately before spawn.
/// That separation lets tests prepare non-existent roots without weakening the
/// live preflight contract.
/// The complete prompt is checked against action-local UTF-8 byte limits here;
/// neither preparation nor later rendering truncates it to fit the cap.
pub fn prepare_invocation(
    snapshot: &HarnessConfigSnapshot,
    request: InvocationRequest,
) -> Result<PreparedInvocation> {
    validate_alias(&request.alias)?;
    validate_absolute_lexical_path(&request.project_root, "project root")?;
    if request.prompt.is_empty() {
        bail!("invocation prompt 不能为空");
    }
    if !is_full_lower_hex_head(&request.target_head) {
        bail!("target head 必须是 40 位小写十六进制 full HEAD");
    }

    let resolved = snapshot.resolve(&request.alias, request.action.config_action())?;
    resolved.limits().validate_prompt(&request.prompt)?;
    let driver_contract = resolved
        .driver()
        .driver_contract(request.action.driver_action())
        .with_context(|| {
            format!(
                "driver {} 不支持 {}",
                resolved.driver().as_str(),
                request.action.as_str()
            )
        })?;
    let (cwd, cwd_selection) = match resolved.cwd_policy() {
        HarnessCwdPolicy::ProjectRoot => (request.project_root.clone(), CwdSelection::ProjectRoot),
        HarnessCwdPolicy::TargetWorktree => {
            validate_absolute_lexical_path(&request.target_worktree, "target worktree")?;
            (
                request.target_worktree.clone(),
                CwdSelection::TargetWorktree,
            )
        }
    };
    if request.action == InvocationAction::Review {
        validate_absolute_lexical_path(&request.target_worktree, "review target worktree")?;
    }

    let tuple = tuple_from_resolved(&resolved);
    let executable_identity = capture_executable_identity(resolved.executable())?;
    let requested = RequestedInvocation {
        action: request.action,
        tuple: tuple.clone(),
    };
    let request_digest = request_digest(
        snapshot,
        &request,
        &resolved,
        &tuple,
        &executable_identity,
        driver_contract,
        cwd_selection,
        &cwd,
    );

    Ok(PreparedInvocation {
        alias: request.alias,
        driver: resolved.driver(),
        executable: resolved.executable().to_path_buf(),
        cwd,
        cwd_selection,
        prompt: request.prompt,
        project_root: request.project_root,
        target_worktree: request.target_worktree,
        target_head: request.target_head,
        attachments: request.attachments,
        config_digest: snapshot.sha256().to_owned(),
        config_source: snapshot.source_path().to_path_buf(),
        request_digest,
        requested,
        effective: tuple,
        limits: resolved.limits().clone(),
        driver_contract,
        executable_identity,
    })
}

fn tuple_from_resolved(resolved: &ResolvedHarness) -> InvocationTuple {
    InvocationTuple {
        provider: resolved.provider().map(str::to_owned),
        model: resolved.model().map(str::to_owned),
        effort: resolved.effort().map(str::to_owned),
        mode: resolved.mode().map(str::to_owned),
    }
}

fn request_digest(
    snapshot: &HarnessConfigSnapshot,
    request: &InvocationRequest,
    resolved: &ResolvedHarness,
    tuple: &InvocationTuple,
    executable_identity: &ExecutableIdentityV1,
    contract: DriverContract,
    cwd_selection: CwdSelection,
    cwd: &Path,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"orch-unified-invocation-request-v1\0");
    update_bytes(&mut digest, snapshot.sha256().as_bytes());
    update_bytes(&mut digest, snapshot.source_path().as_os_str().as_bytes());
    update_bytes(&mut digest, request.alias.as_bytes());
    update_bytes(&mut digest, request.action.as_str().as_bytes());
    update_bytes(&mut digest, resolved.driver().as_str().as_bytes());
    update_bytes(&mut digest, resolved.executable().as_os_str().as_bytes());
    update_bytes(&mut digest, executable_identity.sha256().as_bytes());
    update_optional(&mut digest, tuple.provider.as_deref());
    update_optional(&mut digest, tuple.model.as_deref());
    update_optional(&mut digest, tuple.effort.as_deref());
    update_optional(&mut digest, tuple.mode.as_deref());
    update_bytes(
        &mut digest,
        match contract.transport {
            HarnessTransport::CliDirect => b"cli-direct",
            HarnessTransport::CliStream => b"cli-stream",
            HarnessTransport::DaemonHttp => b"daemon-http",
            HarnessTransport::SocketInject => b"socket-inject",
            HarnessTransport::HttpInject => b"http-inject",
        },
    );
    update_optional(&mut digest, contract.wrapper);
    update_bytes(&mut digest, contract.receipt.as_str().as_bytes());
    update_bytes(&mut digest, contract.terminal.as_str().as_bytes());
    update_bytes(&mut digest, contract.activity.as_str().as_bytes());
    update_bytes(&mut digest, contract.observation_source.as_bytes());
    for value in [
        contract.control.status,
        contract.control.cancel,
        contract.control.attach,
        contract.control.reissue,
        contract.control.declare_dead,
    ] {
        digest.update([u8::from(value)]);
    }
    update_bytes(&mut digest, request.prompt.as_bytes());
    update_bytes(&mut digest, request.project_root.as_os_str().as_bytes());
    update_bytes(&mut digest, request.target_worktree.as_os_str().as_bytes());
    update_bytes(&mut digest, request.target_head.as_bytes());
    update_bytes(&mut digest, cwd_selection.as_str().as_bytes());
    update_bytes(&mut digest, cwd.as_os_str().as_bytes());
    update_bytes(&mut digest, request.attachments.sha256().as_bytes());
    hex::encode(digest.finalize())
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

fn validate_absolute_lexical_path(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{label} 必须是绝对路径");
    }
    let raw = path.as_os_str().as_bytes();
    if raw.len() > 1 && (raw.ends_with(b"/") || raw.windows(2).any(|pair| pair == b"//")) {
        bail!("{label} 必须是唯一 lexical spelling，禁止重复或尾随 /");
    }
    if raw
        .split(|byte| *byte == b'/')
        .any(|segment| matches!(segment, b"." | b".."))
    {
        bail!("{label} 必须是无 . 或 .. 的 lexical absolute path");
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    }) {
        bail!("{label} 必须是无 . 或 .. 的 lexical absolute path");
    }
    Ok(())
}

fn is_full_lower_hex_head(head: &str) -> bool {
    head.len() == 40
        && head
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathIdentity {
    dev: u64,
    ino: u64,
    mode: u32,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl PathIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            len: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

fn capture_executable_identity(path: &Path) -> Result<ExecutableIdentityV1> {
    capture_file_identity(path, true)
}

fn capture_code_owned_wrapper_identity(path: &Path) -> Result<ExecutableIdentityV1> {
    capture_file_identity(path, false)
}

fn capture_file_identity(path: &Path, require_executable: bool) -> Result<ExecutableIdentityV1> {
    validate_absolute_lexical_path(path, "harness executable")?;
    let configured_before = fs::symlink_metadata(path)
        .with_context(|| format!("lstat harness executable 失败: {}", path.display()))?;
    if !(configured_before.file_type().is_symlink() || configured_before.is_file()) {
        bail!("harness executable 必须是 regular file 或稳定 symlink");
    }
    let resolved_before = fs::canonicalize(path)
        .with_context(|| format!("解析 harness executable 失败: {}", path.display()))?;
    let target_before = fs::symlink_metadata(&resolved_before).with_context(|| {
        format!(
            "lstat resolved harness executable 失败: {}",
            resolved_before.display()
        )
    })?;
    if target_before.file_type().is_symlink()
        || !target_before.is_file()
        || (require_executable && target_before.mode() & 0o111 == 0)
    {
        bail!(
            "resolved harness path 必须是{} regular no-symlink file: {}",
            if require_executable {
                " executable"
            } else {
                ""
            },
            resolved_before.display()
        );
    }
    let file = File::open(&resolved_before).with_context(|| {
        format!(
            "打开 resolved harness executable 失败: {}",
            resolved_before.display()
        )
    })?;
    let opened = file.metadata().context("fstat harness executable 失败")?;
    require_same_file(&target_before, &opened, "harness executable open 前后")?;

    let configured_after = fs::symlink_metadata(path)
        .with_context(|| format!("再次 lstat harness executable 失败: {}", path.display()))?;
    let resolved_after = fs::canonicalize(path)
        .with_context(|| format!("再次解析 harness executable 失败: {}", path.display()))?;
    let target_after = fs::symlink_metadata(&resolved_after).with_context(|| {
        format!(
            "再次 lstat resolved harness executable 失败: {}",
            resolved_after.display()
        )
    })?;
    if PathIdentity::from_metadata(&configured_before)
        != PathIdentity::from_metadata(&configured_after)
        || resolved_before != resolved_after
    {
        bail!("harness executable configured path identity 漂移");
    }
    require_same_file(&opened, &target_after, "harness executable capture 期间")?;

    let configured_identity = PathIdentity::from_metadata(&configured_after);
    let target_identity = PathIdentity::from_metadata(&target_after);
    let mut digest = Sha256::new();
    digest.update(b"orch-executable-identity-v1\0");
    update_bytes(&mut digest, path.as_os_str().as_bytes());
    update_path_identity(&mut digest, &configured_identity);
    update_bytes(&mut digest, resolved_after.as_os_str().as_bytes());
    update_path_identity(&mut digest, &target_identity);
    Ok(ExecutableIdentityV1 {
        configured_path: path.to_path_buf(),
        configured_identity,
        resolved_path: resolved_after,
        target_identity,
        sha256: hex::encode(digest.finalize()),
    })
}

fn require_regular_no_symlink_chain(path: &Path) -> Result<(Vec<PathIdentity>, Metadata)> {
    let mut current = PathBuf::new();
    let components = path.components().collect::<Vec<_>>();
    let mut identities = Vec::with_capacity(components.len());
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("lstat attachment 失败: {}", current.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("attachment path 禁止 symlink: {}", current.display());
        }
        identities.push(PathIdentity::from_metadata(&metadata));
        let leaf = index + 1 == components.len();
        if leaf {
            if !metadata.is_file() {
                bail!("attachment leaf 必须是 regular file: {}", current.display());
            }
            return Ok((identities, metadata));
        }
        if !metadata.is_dir() {
            bail!("attachment parent 必须是 directory: {}", current.display());
        }
    }
    bail!("attachment path 不能为空")
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
        bail!("{phase} identity/bytes 漂移");
    }
    Ok(())
}

fn same_path_topology(left: &[PathIdentity], right: &[PathIdentity]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(before, after)| {
            before.dev == after.dev && before.ino == after.ino && before.mode == after.mode
        })
}

fn update_len(digest: &mut Sha256, len: usize) {
    digest.update((len as u64).to_be_bytes());
}

fn update_bytes(digest: &mut Sha256, bytes: &[u8]) {
    update_len(digest, bytes.len());
    digest.update(bytes);
}

fn update_optional(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            update_bytes(digest, value.as_bytes());
        }
        None => digest.update([0]),
    }
}

fn update_path_identity(digest: &mut Sha256, identity: &PathIdentity) {
    digest.update(identity.dev.to_be_bytes());
    digest.update(identity.ino.to_be_bytes());
    digest.update(identity.mode.to_be_bytes());
    digest.update(identity.len.to_be_bytes());
    digest.update(identity.mtime.to_be_bytes());
    digest.update(identity.mtime_nsec.to_be_bytes());
    digest.update(identity.ctime.to_be_bytes());
    digest.update(identity.ctime_nsec.to_be_bytes());
}

#[cfg(test)]
mod b322_operational_environment_tests {
    use super::*;

    #[test]
    fn controlled_environment_scopes_login_identity_to_claude_without_credentials() {
        let claude = controlled_operational_environment(HarnessId::Claude).unwrap();
        if let Some(expected) = std::env::var_os("HOME") {
            assert_eq!(claude.get("HOME").map(String::as_str), expected.to_str());
        }
        if let Some(expected) = std::env::var_os("USER") {
            assert_eq!(claude.get("USER").map(String::as_str), expected.to_str());
        }
        assert_eq!(
            claude.get("PATH").map(String::as_str),
            Some("/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin")
        );
        for driver in [HarnessId::Claude, HarnessId::OpenCode, HarnessId::Dsh] {
            let environment = controlled_operational_environment(driver).unwrap();
            if driver != HarnessId::Claude {
                assert!(!environment.contains_key("USER"));
            }
            for credential in [
                "ANTHROPIC_API_KEY",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "SSH_AUTH_SOCK",
            ] {
                assert!(!environment.contains_key(credential));
            }
        }
    }

    #[test]
    fn attachment_parent_timestamp_churn_is_not_topology_drift() {
        let before = PathIdentity {
            dev: 1,
            ino: 2,
            mode: 0o40755,
            len: 3,
            mtime: 4,
            mtime_nsec: 5,
            ctime: 6,
            ctime_nsec: 7,
        };
        let mut after = before.clone();
        after.len = 99;
        after.mtime = 100;
        after.ctime_nsec = 101;
        assert!(same_path_topology(&[before.clone()], &[after.clone()]));
        after.ino += 1;
        assert!(!same_path_topology(&[before], &[after]));
    }

    #[test]
    fn attachment_leaf_identity_and_length_drift_remain_fail_closed() {
        let root = crate::util::test_scratch_dir("b322-channel-leaf-drift");
        fs::create_dir_all(&root).unwrap();
        let first = root.join("first.txt");
        let second = root.join("second.txt");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"two").unwrap();
        let first_before = fs::metadata(&first).unwrap();
        let second_metadata = fs::metadata(&second).unwrap();
        assert!(require_same_file(&first_before, &second_metadata, "inode drift").is_err());

        fs::write(&first, b"longer bytes").unwrap();
        let first_after = fs::metadata(&first).unwrap();
        assert!(require_same_file(&first_before, &first_after, "length drift").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

/// Serialize capacity-consuming runtime entry points across processes. The
/// lock covers admission through durable publication, closing the otherwise
/// unavoidable read/spawn/append race between dispatch and review wake.
pub fn with_capacity_lock<T>(root: &Path, action: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&lock_dir).context("创建 capacity lock 目录失败")?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_dir.join("capacity.lock"))
        .context("打开 capacity lock 失败")?;
    let mut lock = fd_lock::RwLock::new(lock_file);
    let _guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            bail!("capacity admission lease busy：fail-fast 拒绝并发派发/审查注入")
        }
        Err(error) => return Err(error).context("获取 capacity admission lease 失败"),
    };
    action()
}
