//! 收取阶段（run-task 与 await-report 共用）：REPORT 真值检查 → 机检 → 先红复跑 → 门 → 试合并门 → 落账。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::{binding, buildcache, card, gate, gitx, ledger, legacy, mech, oracle, plan};
use anyhow::{bail, Context, Result};

const GATE_EXECUTED_SCHEMA: (&str, &[&str]) = (
    "GateExecuted",
    &[
        "commandRef",
        "phase",
        "gateRunId",
        "exitCode",
        "durationMs",
        "subjectTreeSha",
        "logSha256",
        "logBytes",
        "toolchainDigest",
        "environmentDigest",
    ],
);

// Only collect owns this independent repository. Tests that clone source_root
// and select origin/main must see the same tree that their binary was built from.
pub(crate) struct CollectSourceSnapshot {
    root: PathBuf,
    path: PathBuf,
    parent_identity: (u64, u64),
    identity: (u64, u64),
    git_identity: (u64, u64),
    pinned_main: String,
    closed: bool,
}

fn collect_directory_identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "collect snapshot requires a regular directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    bail!("collect snapshot directory identity is unavailable on this platform")
}

fn collect_snapshot_git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .output()
        .context("collect snapshot Git command failed to start")?;
    if !output.status.success() {
        bail!(
            "collect snapshot Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

impl CollectSourceSnapshot {
    pub(crate) fn new(
        root: &Path,
        head: &str,
        _permit: &crate::storage::StoragePermit,
    ) -> Result<Self> {
        if head.len() != 40
            || !head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("collect snapshot requires a fixed full commit SHA");
        }
        let root = fs::canonicalize(root)?;
        if collect_snapshot_git(&root, &["rev-parse", &format!("{head}^{{commit}}")])? != head {
            bail!("collect snapshot candidate is not an exact commit");
        }
        let parent = root.join(".cowork-temp");
        fs::create_dir_all(&parent)?;
        let parent_identity = collect_directory_identity(&parent)?;
        let path = parent.join(crate::util::unique_scratch_name("collect-source"));
        if fs::symlink_metadata(&path).is_ok() {
            bail!("collect snapshot refuses to reuse {}", path.display());
        }
        // The caller already holds its collect storage permit. Refs, objects and
        // the default Cargo target belong to this clone, without shared alternates.
        collect_snapshot_git(
            &root,
            &[
                "clone",
                "--quiet",
                "--no-local",
                "--no-checkout",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "core.fsmonitor=false",
                root.to_str().context("root path is not UTF-8")?,
                path.to_str().context("snapshot path is not UTF-8")?,
            ],
        )
        .with_context(|| {
            format!(
                "partial collect snapshot, if present, retained at {}",
                path.display()
            )
        })?;
        let identity = collect_directory_identity(&path)?;
        let git_identity = collect_directory_identity(&path.join(".git"))?;
        let pinned_main = collect_snapshot_git(&path, &["rev-parse", "refs/heads/main^{commit}"])?;
        collect_snapshot_git(&path, &["checkout", "--quiet", "--detach", &pinned_main])?;
        let mut source = Self {
            root,
            path,
            parent_identity,
            identity,
            git_identity,
            pinned_main,
            closed: false,
        };
        source.select(head)?;
        Ok(source)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn validate_identity(&self) -> Result<()> {
        if self.closed
            || self.path.parent() != Some(self.root.join(".cowork-temp").as_path())
            || collect_directory_identity(self.path.parent().context("snapshot parent missing")?)?
                != self.parent_identity
            || collect_directory_identity(&self.path)? != self.identity
            || collect_directory_identity(&self.path.join(".git"))? != self.git_identity
        {
            bail!("collect snapshot identity changed; preserve replacement");
        }
        for relative in [".git/objects", ".git/refs", ".git/refs/heads"] {
            collect_directory_identity(&self.path.join(relative))?;
        }
        if fs::symlink_metadata(self.path.join(".git/objects/info/alternates")).is_ok() {
            bail!("collect snapshot must not borrow external object storage");
        }
        for relative in [".git/HEAD", ".git/packed-refs", ".git/refs/heads/main"] {
            match fs::symlink_metadata(self.path.join(relative)) {
                Ok(m) if !m.is_file() || m.file_type().is_symlink() => {
                    bail!("collect snapshot ref file is not regular")
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        let common = PathBuf::from(collect_snapshot_git(
            &self.path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        let root_common = PathBuf::from(collect_snapshot_git(
            &self.root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        if fs::canonicalize(&common)? != fs::canonicalize(self.path.join(".git"))?
            || fs::canonicalize(common)? == fs::canonicalize(root_common)?
        {
            bail!("collect snapshot must own independent Git refs");
        }
        Ok(())
    }

    fn select(&mut self, head: &str) -> Result<()> {
        self.validate_identity()?;
        if head.len() != 40
            || !head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || collect_snapshot_git(&self.path, &["rev-parse", &format!("{head}^{{commit}}")])?
                != head
        {
            bail!("collect snapshot phase requires a fixed full commit SHA");
        }
        let root_main = collect_snapshot_git(&self.root, &["rev-parse", "refs/heads/main"])?;
        let root_head = collect_snapshot_git(&self.root, &["rev-parse", "HEAD"])?;
        let current = collect_snapshot_git(&self.path, &["rev-parse", "HEAD"])?;
        if fs::read_to_string(self.path.join(".git/HEAD"))?.trim() != current {
            bail!("collect snapshot HEAD must stay detached");
        }
        if collect_snapshot_git(&self.path, &["rev-parse", "refs/heads/main"])? != self.pinned_main
            || (current != self.pinned_main && current != head)
        {
            bail!("collect snapshot private references drifted");
        }
        // The oracle already restores detached HEAD to the candidate. That is
        // the only permitted HEAD/main mismatch at this phase boundary.
        collect_snapshot_git(
            &self.path,
            &["update-ref", "refs/heads/main", head, &self.pinned_main],
        )?;
        self.pinned_main = head.to_string();
        collect_snapshot_git(&self.path, &["checkout", "--quiet", "--detach", head])?;
        self.validate_identity()?;
        if collect_snapshot_git(&self.path, &["rev-parse", "HEAD"])? != head
            || collect_snapshot_git(&self.path, &["rev-parse", "refs/heads/main"])? != head
            || collect_snapshot_git(&self.root, &["rev-parse", "refs/heads/main"])? != root_main
            || collect_snapshot_git(&self.root, &["rev-parse", "HEAD"])? != root_head
            || !collect_snapshot_git(
                &self.path,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            )?
            .is_empty()
        {
            bail!("collect snapshot phase failed its fixed-ref postcondition");
        }
        Ok(())
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.validate_identity()?;
        if collect_snapshot_git(&self.path, &["rev-parse", "HEAD"])? != self.pinned_main
            || collect_snapshot_git(&self.path, &["rev-parse", "refs/heads/main"])?
                != self.pinned_main
            || !collect_snapshot_git(
                &self.path,
                &["status", "--porcelain=v1", "--untracked-files=all"],
            )?
            .is_empty()
        {
            bail!(
                "collect snapshot source or refs changed; retained {}",
                self.path.display()
            );
        }
        let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
            .context("lsof unavailable; collect snapshot retained")?;
        let output = Command::new(lsof)
            .args(["-nP", "-t", "+D"])
            .arg(&self.path)
            .output()?;
        if output.status.code() != Some(1) || !output.stdout.is_empty() || !output.stderr.is_empty()
        {
            bail!(
                "collect snapshot has open files or uncertain use; retained {}",
                self.path.display()
            );
        }
        self.validate_identity()?;
        crate::util::remove_dir_all_with_enotempty_retry(&self.path)?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for CollectSourceSnapshot {
    fn drop(&mut self) {
        if !self.closed {
            eprintln!(
                "collect snapshot retained for incomplete/failed collection at {}",
                self.path.display()
            );
        }
    }
}

fn finish_collect_red_phase(
    source: &mut CollectSourceSnapshot,
    candidate: &str,
    replay: Result<()>,
) -> Result<()> {
    // Oracle may already have restored detached HEAD even on an error. Preserve
    // that fact; an error never authorizes another phase or source deletion.
    replay?;
    source.select(candidate)
}

pub(crate) fn reject_collect_source_redirects() -> Result<()> {
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "CARGO_TARGET_DIR",
    ] {
        if std::env::var_os(key).is_some_and(|value| !value.is_empty()) {
            bail!(
                "collect requires default Git/Cargo source context; ambient {key} is unsupported"
            );
        }
    }
    Ok(())
}

/// Mechanically checked collect result returned to the durable receipt layer.
pub struct CollectOutcome {
    /// Human-readable machine-check notes emitted before any gate runs.
    pub mech_notes: Vec<String>,
    /// Ordered collect-lane gates only; red replay and trial gates are accounted separately.
    pub gates: Vec<gate::GateResult>,
}

#[derive(Debug)]
struct ResolvedCollectCommand {
    name: String,
    spec: binding::CommandSpec,
    digest_input: plan::ResolvedCommandArgvV1,
}

#[derive(Debug)]
/// Internal execution plan shared verbatim by collect and durable receipt replay.
pub(crate) struct CollectLanePlan {
    commands: Vec<ResolvedCollectCommand>,
    trial_refs: Vec<String>,
    committed_binding: binding::Binding,
    candidate_narrow: bool,
    escalation: Option<GateLaneEscalation>,
    resolved_command_digest: String,
    source_reader_descriptor_sha256: Option<String>,
    source_reader_base_sha256: Option<String>,
}

#[derive(Debug)]
struct GateLaneEscalation {
    reason: String,
    resolved_command_digest: String,
}

impl CollectLanePlan {
    /// Return the exact invocation sequence without collapsing repeated semantic command refs.
    pub(crate) fn ordered_command_refs(&self) -> Vec<String> {
        self.commands
            .iter()
            .map(|command| command.name.clone())
            .collect()
    }

    /// Borrow the policy-base argv digest shared by gate execution, receipts, and root reuse.
    pub(crate) fn resolved_command_digest(&self) -> &str {
        &self.resolved_command_digest
    }

    /// Borrow authenticated descriptor/base digests when the active candidate closure used them.
    pub(crate) fn source_reader_digests(&self) -> Option<(&str, &str)> {
        Some((
            self.source_reader_descriptor_sha256.as_deref()?,
            self.source_reader_base_sha256.as_deref()?,
        ))
    }

    /// Report whether this plan depends on the attempt's monotonic typed escalation fact.
    pub(crate) fn requires_escalation_event(&self) -> bool {
        self.escalation.is_some()
    }
}

fn copy_command_spec(spec: &binding::CommandSpec) -> binding::CommandSpec {
    binding::CommandSpec {
        argv: spec.argv.clone(),
        timeout_seconds: spec.timeout_seconds,
        trial_timeout_seconds: spec.trial_timeout_seconds,
        approval: spec.approval.clone(),
    }
}

fn static_collect_commands(
    binding: &binding::Binding,
    refs: &[String],
) -> Result<Vec<ResolvedCollectCommand>> {
    refs.iter()
        .map(|command_ref| {
            let spec = binding
                .commands
                .get(command_ref)
                .with_context(|| format!("policy-base binding 缺 command {command_ref}"))?;
            Ok(ResolvedCollectCommand {
                name: command_ref.clone(),
                spec: copy_command_spec(spec),
                digest_input: plan::ResolvedCommandArgvV1 {
                    command_ref: command_ref.clone(),
                    binding_command_ref: command_ref.clone(),
                    derived_argv: Vec::new(),
                },
            })
        })
        .collect()
}

fn resolved_collect_command_digest(
    root: &Path,
    policy_base_sha: &str,
    commands: &[ResolvedCollectCommand],
) -> Result<String> {
    plan::resolved_command_argv_digest_at_policy_base(
        root,
        policy_base_sha,
        &commands
            .iter()
            .map(|command| command.digest_input.clone())
            .collect::<Vec<_>>(),
    )
}

fn integration_selector(target: &str) -> std::result::Result<(String, String), String> {
    let path = Path::new(target);
    if target.is_empty()
        || path.is_absolute()
        || !path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "seed target 非 canonical repo-relative path: {target:?}"
        ));
    }
    let parts = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ["orch", "crates", package, "tests", file] = parts.as_slice() else {
        return Err(format!(
            "seed target 无法转换为 Cargo integration target: {target}"
        ));
    };
    let Some(test) = file.strip_suffix(".rs") else {
        return Err(format!("seed target 不是 .rs: {target}"));
    };
    let safe = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if !safe(package) || !safe(test) {
        return Err(format!(
            "seed target package/test 非安全 component: {target}"
        ));
    }
    Ok(((*package).to_string(), test.to_string()))
}

fn derived_test_command(
    binding: &binding::Binding,
    command_ref: &str,
    binding_command_ref: &str,
    package: &str,
    test: &str,
) -> Result<ResolvedCollectCommand> {
    let base = binding.commands.get(binding_command_ref).with_context(|| {
        format!("policy-base binding 缺 derived command base {binding_command_ref}")
    })?;
    if base.argv.iter().any(|arg| arg == "--") {
        bail!("derived Cargo command base 不得在 selector 前含 `--` terminator");
    }
    let derived_argv = vec![
        "-p".to_string(),
        package.to_string(),
        "--test".to_string(),
        test.to_string(),
    ];
    let mut spec = copy_command_spec(base);
    spec.argv.extend(derived_argv.iter().cloned());
    Ok(ResolvedCollectCommand {
        // GateExecuted.commandRef remains the semantic V1 reference. The
        // phase-scoped gateRunId already makes multiple derived invocations'
        // log stems unique; package/test live in the separately recomputed
        // resolved-command digest rather than inventing an unbound ref.
        name: command_ref.to_string(),
        spec,
        digest_input: plan::ResolvedCommandArgvV1 {
            command_ref: command_ref.to_string(),
            binding_command_ref: binding_command_ref.to_string(),
            derived_argv,
        },
    })
}

fn normalize_candidate_refs(binding: &binding::Binding) -> (Vec<String>, Option<String>) {
    let shape_count = binding
        .gates
        .candidate
        .iter()
        .filter(|value| value.as_str() == "shapeTests")
        .count();
    let closure_count = binding
        .gates
        .candidate
        .iter()
        .filter(|value| value.as_str() == "sourceReaderClosure")
        .count();
    if shape_count == 1 && closure_count == 0 {
        return (
            binding
                .gates
                .candidate
                .iter()
                .map(|value| {
                    if value == "shapeTests" {
                        "sourceReaderClosure".to_string()
                    } else {
                        value.clone()
                    }
                })
                .collect(),
            None,
        );
    }
    if shape_count > 0 {
        return (
            binding.gates.candidate.clone(),
            Some(
                "legacy shapeTests slot must occur exactly once and cannot coexist with sourceReaderClosure"
                    .to_string(),
            ),
        );
    }
    (binding.gates.candidate.clone(), None)
}

fn candidate_known_commands(binding: &binding::Binding) -> Vec<String> {
    let mut known = binding.commands.keys().cloned().collect::<Vec<_>>();
    // sourceReaderClosure is a virtual semantic command whose only modeled
    // executable base is the committed seedTargets prefix. Never synthesize
    // seedTargets itself: deleting that command must trigger a fast upgrade.
    if binding.commands.contains_key("seedTargets")
        && !known.iter().any(|value| value == "sourceReaderClosure")
    {
        known.push("sourceReaderClosure".to_string());
    }
    known
}

fn build_closed_candidate_commands(
    binding: &binding::Binding,
    seed_selectors: &[(String, String)],
    closure_targets: &[legacy::SourceReaderTargetV1],
) -> Result<Vec<ResolvedCollectCommand>> {
    let mut commands = Vec::new();
    for (package, test) in seed_selectors {
        commands.push(derived_test_command(
            binding,
            "seedTargets",
            "seedTargets",
            package,
            test,
        )?);
    }
    for target in closure_targets {
        commands.push(derived_test_command(
            binding,
            &target.command_ref,
            "seedTargets",
            &target.package,
            &target.test,
        )?);
    }
    commands.extend(static_collect_commands(binding, &["check".to_string()])?);
    Ok(commands)
}

/// Resolve the signed merge lane while preserving the task card's fast-gate floor.
///
/// Root and trial callers share this boundary so a later binding cannot make either phase
/// weaker than the exact ordered command set the user signed in the task card.
pub(crate) fn resolved_merge_gate_refs_for_signed_fast(
    binding: &binding::Binding,
    signed_fast: &[String],
    available: &[String],
) -> Result<Vec<String>> {
    if binding.gates.candidate.is_empty()
        && binding.gates.merge.is_empty()
        && binding.gates.fast.is_empty()
    {
        return binding::resolve_gates(signed_fast, available).map_err(anyhow::Error::msg);
    }
    let mut known_commands = available.to_vec();
    if binding.commands.contains_key("seedTargets")
        && !known_commands
            .iter()
            .any(|known| known == "sourceReaderClosure")
    {
        known_commands.push("sourceReaderClosure".to_string());
    }
    match binding::resolve_gate_lane_v1(&binding::GateLaneInputsV1 {
        lane: binding::GateLaneV1::Merge,
        candidate: binding.gates.candidate.clone(),
        merge: binding.gates.merge.clone(),
        fast: binding.gates.fast.clone(),
        card_fast: signed_fast.to_vec(),
        known_commands,
        seed_targets_closed: false,
        source_readers_closed: false,
    })
    .map_err(anyhow::Error::msg)?
    {
        binding::GateLaneDecisionV1::Commands(refs) => Ok(refs),
        binding::GateLaneDecisionV1::UpgradeToFast { .. } => {
            bail!("merge lane 不得返回 candidate escalation")
        }
    }
}

fn resolved_merge_gate_refs(
    binding: &binding::Binding,
    card: &card::Card,
    available: &[String],
) -> Result<Vec<String>> {
    resolved_merge_gate_refs_for_signed_fast(binding, &card.meta.gates.fast, available)
}

fn final_tree_policy_active(
    root: &Path,
    round: &str,
    events: &[orch_core::EventRecord],
    task_id: &str,
    attempt_id: &str,
    policy_base_sha: &str,
) -> Result<bool> {
    let binding_bytes =
        gitx::show_bytes(root, policy_base_sha, "coordination/PROJECT-BINDING.yaml")?;
    let binding_yaml: serde_yaml::Value = serde_yaml::from_slice(&binding_bytes)
        .context("final-tree policy-base binding 非 canonical")?;
    let declared = binding_yaml
        .get("runtimePolicies")
        .and_then(|value| value.get("policies"))
        .and_then(|value| value.get("final-tree-v1"))
        .is_some();
    if !declared {
        return Ok(false);
    }
    match plan::resolve_attempt_runtime_policy(
        root,
        round,
        events,
        task_id,
        attempt_id,
        "final-tree-v1",
    ) {
        Ok(resolution) => {
            if resolution.policy_base_sha != policy_base_sha {
                bail!("final-tree supplied policyBaseSha 与 DispatchIssued.baseSha 不一致");
            }
            Ok(resolution.state == plan::RuntimePolicyStateV1::Active)
        }
        Err(error)
            if error
                .to_string()
                .contains("缺 runtime policy final-tree-v1")
                || error.to_string().contains("缺 runtimePolicies envelope")
                || {
                    let detail = format!("{error:#}");
                    (detail.contains("读取 policy-base committed PROJECT-BINDING 失败")
                        || detail.contains("读取 policy-base committed ROUND-IR 失败"))
                        && (detail.contains("does not exist in")
                            || detail.contains("exists on disk, but not in"))
                } =>
        {
            Ok(false)
        }
        Err(error) => Err(error).context("resolve attempt final-tree-v1 policy failed"),
    }
}

fn append_gate_lane_escalation_once(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    attempt_no: usize,
    policy_base_sha: &str,
    escalation: &GateLaneEscalation,
) -> Result<()> {
    let task_id = task_id.to_string();
    let attempt_id = attempt_id.to_string();
    let policy_base_sha = policy_base_sha.to_string();
    let reason = escalation.reason.clone();
    let resolved_command_digest = escalation.resolved_command_digest.clone();
    let appended = ledger::append_checked(root, round, move |events| {
        let prior = events
            .iter()
            .filter(|event| {
                event.kind == "GateLaneEscalated"
                    && event.task_id.as_deref() == Some(task_id.as_str())
                    && event.round.as_deref() == Some(round)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("attemptId"))
                        .and_then(serde_json::Value::as_str)
                        == Some(attempt_id.as_str())
            })
            .collect::<Vec<_>>();
        if prior.iter().any(|event| {
            event.payload.as_ref().is_some_and(|payload| {
                payload.get("reason").and_then(serde_json::Value::as_str) == Some(reason.as_str())
                    && payload
                        .get("resolvedCommandDigest")
                        .and_then(serde_json::Value::as_str)
                        == Some(resolved_command_digest.as_str())
                    && payload
                        .get("policyBaseSha")
                        .and_then(serde_json::Value::as_str)
                        == Some(policy_base_sha.as_str())
            })
        }) {
            return Ok(Vec::new());
        }
        if !prior.is_empty() {
            bail!("current attempt 已有不同 identity 的 GateLaneEscalated");
        }
        Ok(vec![ledger::runtime_event_v1(
            round,
            Some(&task_id),
            ledger::RuntimeEventPayloadV1::GateLaneEscalated(ledger::GateLaneEscalatedPayloadV1 {
                schema_version: ledger::RUNTIME_EVENT_SCHEMA_V1,
                attempt_id,
                attempt_no,
                from_lane: "candidate".to_string(),
                to_lane: "fast".to_string(),
                reason,
                policy_base_sha,
                resolved_command_digest,
            }),
        )?])
    })?;
    if appended > 1 {
        bail!("GateLaneEscalated append count 非 canonical: {appended}");
    }
    Ok(())
}

/// Resolve the exact ordered collect invocations from the current signed card and immutable base.
///
/// `candidate_sha` supplies replay-stable candidate bytes. The card is deliberately supplied by
/// the active signed revision rather than reloaded from `policy_base_sha`: a successor attempt may
/// inherit its original base while a newly signed revision expands that attempt's legal write set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_collect_lane_plan(
    root: &Path,
    round: &str,
    card: &card::Card,
    events: &[orch_core::EventRecord],
    attempt_id: &str,
    policy_base_sha: &str,
    candidate_sha: &str,
    changed_paths: &[String],
) -> Result<CollectLanePlan> {
    let binding_bytes =
        gitx::show_bytes(root, policy_base_sha, "coordination/PROJECT-BINDING.yaml")?;
    let committed = binding::parse_binding_bytes(&binding_bytes)
        .map_err(anyhow::Error::msg)
        .context("解析 policy-base committed binding 失败")?;
    let available = committed.commands.keys().cloned().collect::<Vec<_>>();

    if card.meta.schema_version == Some(plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION) {
        binding::validate_v3_binding_shape(&binding_bytes)?;
        if committed.gates.candidate.is_empty() {
            bail!("schema 3 candidate gate lane 为空");
        }
        let mut commands = Vec::new();
        for command_ref in &committed.gates.candidate {
            if command_ref == "seedTargets" {
                if card.meta.seeds.is_empty() {
                    if card.meta.seed_protocol.as_deref() == Some("seeded-red") {
                        bail!("schema 3 seeded-red candidate 要求至少一个 signed seed");
                    }
                    continue;
                }
                for seed in &card.meta.seeds {
                    let (package, test) =
                        integration_selector(&seed.target).map_err(anyhow::Error::msg)?;
                    commands.push(derived_test_command(
                        &committed,
                        "seedTargets",
                        "seedTargets",
                        &package,
                        &test,
                    )?);
                }
            } else {
                commands.extend(static_collect_commands(
                    &committed,
                    std::slice::from_ref(command_ref),
                )?);
            }
        }
        let trial_refs = resolved_merge_gate_refs_for_signed_fast(
            &committed,
            &card.meta.gates.fast,
            &available,
        )?;
        let resolved_command_digest =
            resolved_collect_command_digest(root, policy_base_sha, &commands)?;
        return Ok(CollectLanePlan {
            commands,
            trial_refs,
            committed_binding: committed,
            candidate_narrow: true,
            escalation: None,
            resolved_command_digest,
            source_reader_descriptor_sha256: None,
            source_reader_base_sha256: None,
        });
    }

    let binding_yaml: serde_yaml::Value =
        serde_yaml::from_slice(&binding_bytes).context("policy-base binding YAML 非 canonical")?;
    let policy_declared = binding_yaml
        .get("runtimePolicies")
        .and_then(|value| value.get("policies"))
        .and_then(|value| value.get("candidate-lanes-v1"))
        .is_some();
    let resolution = if policy_declared {
        let resolution = plan::resolve_attempt_runtime_policy(
            root,
            round,
            events,
            &card.meta.task_id,
            attempt_id,
            "candidate-lanes-v1",
        )?;
        if resolution.policy_base_sha != policy_base_sha {
            bail!("candidate lane supplied policyBaseSha 与 DispatchIssued.baseSha 不一致");
        }
        Some(resolution)
    } else {
        None
    };

    let policy_active = resolution
        .as_ref()
        .is_some_and(|resolution| resolution.state == plan::RuntimePolicyStateV1::Active);
    if !policy_active {
        let refs = binding::resolve_gates(&card.meta.gates.fast, &available)
            .map_err(anyhow::Error::msg)?;
        let trial_refs = if policy_declared {
            resolved_merge_gate_refs(&committed, card, &available)?
        } else {
            refs.clone()
        };
        let commands = static_collect_commands(&committed, &refs)?;
        let resolved_command_digest =
            resolved_collect_command_digest(root, policy_base_sha, &commands)?;
        return Ok(CollectLanePlan {
            commands,
            trial_refs,
            committed_binding: committed,
            candidate_narrow: false,
            escalation: None,
            resolved_command_digest,
            source_reader_descriptor_sha256: None,
            source_reader_base_sha256: None,
        });
    }

    // Escalation is monotonic for an attempt. A later REPORT revision may
    // remove the unknown edge that first triggered it, but the durable fact
    // already selected the stronger signed lane and cannot be silently
    // downgraded on replay.
    let prior_escalations = events
        .iter()
        .filter(|event| {
            event.kind == "GateLaneEscalated"
                && event.task_id.as_deref() == Some(card.meta.task_id.as_str())
                && event.round.as_deref() == Some(round)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
        })
        .collect::<Vec<_>>();
    if prior_escalations.len() > 1 {
        bail!("current attempt 存在重复 GateLaneEscalated");
    }
    if let Some(prior) = prior_escalations.first() {
        if !ledger::canonical_runtime_event_v1(prior, round)? {
            bail!("current attempt GateLaneEscalated envelope 非 canonical");
        }
        let Some(ledger::RuntimeEventPayloadV1::GateLaneEscalated(payload)) =
            ledger::decode_runtime_event_v1(prior)?
        else {
            bail!("current attempt GateLaneEscalated payload 非 typed V1");
        };
        if payload.policy_base_sha != policy_base_sha {
            bail!("current attempt GateLaneEscalated policyBaseSha 漂移");
        }
        let commands = static_collect_commands(&committed, &committed.gates.fast)?;
        let digest = plan::resolved_command_argv_digest_at_policy_base(
            root,
            policy_base_sha,
            &commands
                .iter()
                .map(|command| command.digest_input.clone())
                .collect::<Vec<_>>(),
        )?;
        if digest != payload.resolved_command_digest {
            bail!("current attempt GateLaneEscalated resolved command digest 漂移");
        }
        let mut known_commands = available.clone();
        if !known_commands
            .iter()
            .any(|known| known == "sourceReaderClosure")
        {
            known_commands.push("sourceReaderClosure".to_string());
        }
        let trial_refs = match binding::resolve_gate_lane_v1(&binding::GateLaneInputsV1 {
            lane: binding::GateLaneV1::Merge,
            candidate: committed.gates.candidate.clone(),
            merge: committed.gates.merge.clone(),
            fast: committed.gates.fast.clone(),
            card_fast: card.meta.gates.fast.clone(),
            known_commands,
            seed_targets_closed: false,
            source_readers_closed: false,
        })
        .map_err(anyhow::Error::msg)?
        {
            binding::GateLaneDecisionV1::Commands(refs) => refs,
            binding::GateLaneDecisionV1::UpgradeToFast { .. } => {
                bail!("merge lane 不得返回 candidate escalation")
            }
        };
        let source_reader_digests =
            legacy::archived_source_reader_digests_v1(root, round, policy_base_sha, candidate_sha)
                .ok();
        return Ok(CollectLanePlan {
            commands,
            trial_refs,
            committed_binding: committed,
            candidate_narrow: false,
            escalation: Some(GateLaneEscalation {
                reason: payload.reason,
                resolved_command_digest: digest.clone(),
            }),
            resolved_command_digest: digest,
            source_reader_descriptor_sha256: source_reader_digests
                .as_ref()
                .map(|(descriptor, _)| descriptor.clone()),
            source_reader_base_sha256: source_reader_digests.map(|(_, base)| base),
        });
    }

    let (candidate_refs, alias_error) = normalize_candidate_refs(&committed);
    let source_changed = changed_paths
        .iter()
        .filter(|path| !card.meta.seeds.iter().any(|seed| &seed.target == *path))
        .cloned()
        .collect::<Vec<_>>();
    let closure = legacy::replay_source_reader_closure_v1(
        root,
        round,
        candidate_sha,
        policy_base_sha,
        &source_changed,
    )?;
    let (closure_targets, closure_reason, descriptor_sha256, base_sha256) = match closure {
        legacy::SourceReaderClosureDecisionV1::Closed {
            targets,
            descriptor_sha256,
            base_sha256,
        } => (targets, None, Some(descriptor_sha256), Some(base_sha256)),
        legacy::SourceReaderClosureDecisionV1::UpgradeToFast {
            reason,
            descriptor_sha256,
            base_sha256,
        } => (
            Vec::new(),
            Some(reason),
            (!descriptor_sha256.is_empty()).then_some(descriptor_sha256),
            (!base_sha256.is_empty()).then_some(base_sha256),
        ),
    };

    let mut seed_selectors = Vec::new();
    let mut seed_reason = None;
    if card.meta.seeds.is_empty() {
        seed_reason = Some("candidate seedTargets requires at least one signed seed".to_string());
    } else {
        for seed in &card.meta.seeds {
            match integration_selector(&seed.target) {
                Ok(selector) => seed_selectors.push(selector),
                Err(reason) => {
                    seed_reason = Some(reason);
                    break;
                }
            }
        }
    }
    let expected_candidate = ["seedTargets", "sourceReaderClosure", "check"];
    let candidate_contract_reason = (candidate_refs
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        != expected_candidate)
        .then(|| {
            format!(
                "candidate lane 必须按序为 seedTargets/sourceReaderClosure/check，实际 {:?}",
                candidate_refs
            )
        });

    let (candidate_commands, candidate_construction_reason) =
        match build_closed_candidate_commands(&committed, &seed_selectors, &closure_targets) {
            Ok(commands) => (commands, None),
            Err(error) => (
                Vec::new(),
                Some(format!(
                    "candidate invocation construction failed closed: {error:#}"
                )),
            ),
        };

    let known_commands = candidate_known_commands(&committed);
    let inputs = binding::GateLaneInputsV1 {
        lane: binding::GateLaneV1::Candidate,
        candidate: candidate_refs,
        merge: committed.gates.merge.clone(),
        fast: committed.gates.fast.clone(),
        card_fast: card.meta.gates.fast.clone(),
        known_commands,
        seed_targets_closed: seed_reason.is_none(),
        source_readers_closed: closure_reason.is_none()
            && alias_error.is_none()
            && candidate_contract_reason.is_none()
            && candidate_construction_reason.is_none(),
    };
    let decision = binding::resolve_gate_lane_v1(&inputs).map_err(anyhow::Error::msg)?;
    let trial_refs = match binding::resolve_gate_lane_v1(&binding::GateLaneInputsV1 {
        lane: binding::GateLaneV1::Merge,
        ..inputs.clone()
    })
    .map_err(anyhow::Error::msg)?
    {
        binding::GateLaneDecisionV1::Commands(refs) => refs,
        binding::GateLaneDecisionV1::UpgradeToFast { .. } => {
            bail!("merge lane 不得返回 candidate escalation")
        }
    };

    if let binding::GateLaneDecisionV1::UpgradeToFast { reason } = decision {
        let refs = committed.gates.fast.clone();
        let commands = static_collect_commands(&committed, &refs)?;
        let digest_inputs = commands
            .iter()
            .map(|command| command.digest_input.clone())
            .collect::<Vec<_>>();
        let resolved_command_digest = plan::resolved_command_argv_digest_at_policy_base(
            root,
            policy_base_sha,
            &digest_inputs,
        )?;
        let detailed = [
            Some(reason),
            seed_reason,
            closure_reason,
            alias_error,
            candidate_contract_reason,
            candidate_construction_reason,
        ]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("; ");
        return Ok(CollectLanePlan {
            commands,
            trial_refs,
            committed_binding: committed,
            candidate_narrow: false,
            escalation: Some(GateLaneEscalation {
                reason: detailed,
                resolved_command_digest: resolved_command_digest.clone(),
            }),
            resolved_command_digest,
            source_reader_descriptor_sha256: descriptor_sha256,
            source_reader_base_sha256: base_sha256,
        });
    }

    // Recompute now even though B304 is the first durable consumer. This makes
    // candidate command construction fail closed before any child runs if its
    // committed-prefix provenance or derived selector shape drifts.
    let resolved_command_digest =
        resolved_collect_command_digest(root, policy_base_sha, &candidate_commands)?;
    Ok(CollectLanePlan {
        commands: candidate_commands,
        trial_refs,
        committed_binding: committed,
        candidate_narrow: true,
        escalation: None,
        resolved_command_digest,
        source_reader_descriptor_sha256: descriptor_sha256,
        source_reader_base_sha256: base_sha256,
    })
}

/// Recompute a replay-stable collect sequence directly from dispatch base to candidate commit.
///
/// Receipt creation and every later validation call this same boundary, so a moved branch,
/// descriptor change, or selector/order drift cannot be hidden behind a cached command-ref list.
pub(crate) fn resolve_collect_lane_plan_at_candidate(
    root: &Path,
    round: &str,
    card: &card::Card,
    events: &[orch_core::EventRecord],
    attempt_id: &str,
    policy_base_sha: &str,
    candidate_sha: &str,
) -> Result<CollectLanePlan> {
    let changed_paths = gitx::diff_names(root, policy_base_sha, candidate_sha)?;
    resolve_collect_lane_plan(
        root,
        round,
        card,
        events,
        attempt_id,
        policy_base_sha,
        candidate_sha,
        &changed_paths,
    )
}

fn note_gate_orphan_evidence(log_dir: &Path, tag: &str, gate_name: &str, observation: &str) {
    if let Err(error) = gate::append_gate_orphan_evidence(log_dir, tag, gate_name, observation) {
        eprintln!(
            "gate orphan evidence degraded to stderr for {tag}/{gate_name}: {error:#}; {observation}"
        );
    }
}

fn collect_orphan_baseline(log_dir: &Path, tag: &str, gate_name: &str) -> Option<BTreeSet<u32>> {
    match gate::orphan_baseline() {
        Ok(baseline) => Some(baseline),
        Err(error) => {
            note_gate_orphan_evidence(
                log_dir,
                tag,
                gate_name,
                &format!("phase=baseline snapshot=degraded error={error:#}"),
            );
            None
        }
    }
}

fn finish_collect_orphan_watch(
    root: &Path,
    context: Option<(&str, &str)>,
    log_dir: &Path,
    tag: &str,
    gate_name: &str,
    baseline: Option<&BTreeSet<u32>>,
) -> Result<()> {
    let Some(baseline) = baseline else {
        return Ok(());
    };
    let first = match gate::orphans_since(baseline) {
        Ok(rows) => rows,
        Err(error) => {
            note_gate_orphan_evidence(
                log_dir,
                tag,
                gate_name,
                &format!("phase=post-gate sample=first snapshot=degraded error={error:#}"),
            );
            return Ok(());
        }
    };
    std::thread::sleep(Duration::from_millis(50));
    let second = match gate::orphans_since(baseline) {
        Ok(rows) => rows,
        Err(error) => {
            note_gate_orphan_evidence(
                log_dir,
                tag,
                gate_name,
                &format!("phase=post-gate sample=second snapshot=degraded error={error:#}"),
            );
            return Ok(());
        }
    };
    let persistent = gate::persistent_gate_orphans(&first, &second);
    let (reportable, evidence_only) = gate::partition_gate_orphans(&persistent);
    note_gate_orphan_evidence(
        log_dir,
        tag,
        gate_name,
        &format!(
            "phase=post-gate first={first:?} second={second:?} persistent={persistent:?} reportable={reportable:?} evidence_only={evidence_only:?}"
        ),
    );
    if !reportable.is_empty() {
        if let Some((round, task_id)) = context {
            ledger::append(
                root,
                round,
                &[gate::orphan_failure_event(task_id, round, &reportable)],
            )?;
        }
    }
    Ok(())
}

fn with_collect_orphan_watch<T>(
    root: &Path,
    context: Option<(&str, &str)>,
    log_dir: &Path,
    tag: &str,
    gate_name: &str,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let baseline = collect_orphan_baseline(log_dir, tag, gate_name);
    let result = run();
    let observation =
        finish_collect_orphan_watch(root, context, log_dir, tag, gate_name, baseline.as_ref());
    match (result, observation) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(observation_error)) => {
            eprintln!(
                "gate failed and orphan accounting also failed for {tag}/{gate_name}: {observation_error:#}"
            );
            Err(error)
        }
    }
}

/// Scope a caller-owned gate tag exactly once, before either the orphan watcher or the runner sees
/// it.  Passing the scoped value into the closure makes the four sibling artifacts share one stem
/// even for trial/fallback/replay paths whose runners do not themselves carry a round parameter.
#[cfg(test)]
fn with_round_scoped_collect_gate<T>(
    root: &Path,
    context: Option<(&str, &str)>,
    round: &str,
    log_dir: &Path,
    raw_tag: &str,
    gate_name: &str,
    run: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    if context.is_some_and(|(context_round, _)| context_round != round) {
        bail!("collect gate round scope 与 orphan context 不一致");
    }
    let scoped_tag = gate::round_scoped_log_tag(round, raw_tag);
    with_collect_orphan_watch(root, context, log_dir, &scoped_tag, gate_name, || {
        run(&scoped_tag)
    })
}

/// 收取期试合并的不可变计划。两个输入都是 concrete SHA，scratch 是仓内
/// detached worktree；计划本身没有任何可移动 ref。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialMergePlan {
    pub scratch_worktree: String,
    pub attempt_head: String,
    pub main_head: String,
}

/// 试合并门的可审计结论。只有 [`Green`](Self::Green) 可继续 collect；
/// 红门必须携带门名、退出码、同轮提交和原始失败详情，冲突必须携带文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrialMergeVerdict {
    Green,
    Red {
        gate: String,
        exit_code: i32,
        interacting_commits: Vec<String>,
        detail: String,
    },
    Conflict {
        files: Vec<String>,
    },
}

impl TrialMergeVerdict {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Green)
    }

    pub fn render(&self) -> String {
        match self {
            Self::Green => "收取期试合并门通过".to_string(),
            Self::Red {
                gate,
                exit_code,
                interacting_commits,
                detail,
            } => format!(
                "收取期试合并门拒绝：门 {gate} 红（exit {exit_code}）；\
                 同轮交互提交：{}；失败详情：\n{detail}",
                render_items(interacting_commits)
            ),
            Self::Conflict { files } => format!(
                "收取期试合并冲突，拒绝 collect；冲突文件：{}",
                render_items(files)
            ),
        }
    }
}

fn render_items(items: &[String]) -> String {
    if items.is_empty() {
        "<无法解析>".to_string()
    } else {
        items.join(", ")
    }
}

/// 精确触发条件：当前 main concrete SHA 与本 attempt 的派发基线不同。
/// 生产路径还会证明 base 是 current main 的祖先，拒绝把 ref 倒退/改写误称为
/// “同轮推进”。
pub fn trial_merge_needed(base_sha: &str, current_main_sha: &str) -> bool {
    base_sha != current_main_sha
}

/// 构造仓内、一次性、detached 的 scratch 计划。task id 先做 component
/// 校验，防止路径穿越；两个 SHA 只作为 git 对象名使用，不成为分支名。
pub fn trial_merge_plan(
    root: &str,
    task_id: &str,
    attempt_head: &str,
    main_head: &str,
) -> Result<TrialMergePlan> {
    card::validate_task_id(task_id)?;
    let root = Path::new(root);
    if !root.is_absolute() {
        bail!("试合并仓根必须是绝对路径: {}", root.display());
    }
    if attempt_head.is_empty() || main_head.is_empty() {
        bail!("试合并必须绑定非空 attempt/main concrete SHA");
    }
    let scratch =
        root.join(".cowork-temp")
            .join(format!("trial-{}-{}", task_id, ulid::Ulid::new()));
    Ok(TrialMergePlan {
        scratch_worktree: scratch.to_string_lossy().into_owned(),
        attempt_head: attempt_head.to_string(),
        main_head: main_head.to_string(),
    })
}

struct TrialWorktree<'a> {
    root: &'a Path,
    path: PathBuf,
    active: bool,
}

impl TrialWorktree<'_> {
    fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        gitx::worktree_remove(self.root, &self.path)
            .with_context(|| format!("清理试合并 worktree 失败: {}", self.path.display()))?;
        self.active = false;
        if fs::symlink_metadata(&self.path).is_ok() {
            bail!(
                "试合并 worktree remove 成功后路径仍存在: {}",
                self.path.display()
            );
        }
        Ok(())
    }
}

impl Drop for TrialWorktree<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = gitx::worktree_remove(self.root, &self.path);
        }
    }
}

fn conflict_files(scratch: &Path, stdout: &str, stderr: &str) -> Vec<String> {
    fn push_unique(files: &mut Vec<String>, path: &str) {
        let path = path.trim();
        if !path.is_empty() && !files.iter().any(|seen| seen == path) {
            files.push(path.to_string());
        }
    }

    let mut files = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if !line.starts_with("CONFLICT") {
            continue;
        }
        if let Some((_, path)) = line.rsplit_once("Merge conflict in ") {
            push_unique(&mut files, path);
        } else if let Some(rest) = line.strip_prefix("CONFLICT (modify/delete): ") {
            if let Some((path, _)) = rest.split_once(" deleted in ") {
                push_unique(&mut files, path);
            }
        }
    }
    if files.is_empty() {
        if let Ok(output) = Command::new("git")
            .arg("-C")
            .arg(scratch)
            .args(["diff", "--name-only", "--diff-filter=U"])
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    push_unique(&mut files, line);
                }
            }
        }
    }
    files
}

/// 小日志完整保留；大日志保留关键失败行和尾部，既让错误能点名具体
/// 测试/编译错误，也避免把一次完整 Cargo 日志塞进拒绝文案。按 bytes
/// 截取后用 lossy UTF-8，永不因非 UTF-8 丢掉红证据。
fn gate_failure_detail(log_path: &str) -> String {
    const LIMIT: usize = 16 * 1024;
    match fs::read(log_path) {
        Ok(bytes) if bytes.len() <= LIMIT => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let signals = text
                .lines()
                .filter(|line| {
                    line.contains("FAILED")
                        || line.contains("failures:")
                        || line.contains("panicked at")
                        || line.contains("error[")
                        || line.contains("error:")
                        || line.contains("test result:")
                })
                .take(64)
                .collect::<Vec<_>>()
                .join("\n");
            let start = bytes.len() - LIMIT;
            format!(
                "关键失败行：\n{}\n日志尾部（最后 {LIMIT} bytes）：\n{}",
                if signals.is_empty() {
                    "<未匹配标准失败行>"
                } else {
                    &signals
                },
                String::from_utf8_lossy(&bytes[start..])
            )
        }
        Err(error) => format!("门日志读取失败（{log_path}）：{error}"),
    }
}

fn interacting_commits(root: &Path, base_sha: &str, main_sha: &str) -> Result<Vec<String>> {
    gitx::commits_after(root, base_sha, main_sha)?
        .into_iter()
        .map(|sha| {
            let subject = gitx::commit_subject(root, &sha)?;
            Ok(format!("{} {}", gitx::short(&sha), subject))
        })
        .collect()
}

fn execute_trial_merge(
    root: &Path,
    round: &str,
    task_id: &str,
    audit_identity: ledger::GateAuditIdentity<'_>,
    storage_permit: &crate::storage::StoragePermit,
    orphan_context: Option<(&str, &str)>,
    plan: &TrialMergePlan,
    commits: Vec<String>,
    gate_refs: &[String],
    commands: &BTreeMap<String, binding::CommandSpec>,
    log_dir: &Path,
) -> Result<TrialMergeVerdict> {
    if buildcache::trial_cargo_program(gate_refs, commands).is_some() {
        return buildcache::with_trial_slot(root, task_id, |slot| {
            let mut fixed_plan = plan.clone();
            fixed_plan.scratch_worktree = slot.source_root().to_string_lossy().into_owned();
            execute_trial_merge_at(
                root,
                round,
                task_id,
                audit_identity,
                storage_permit,
                orphan_context,
                &fixed_plan,
                &commits,
                gate_refs,
                commands,
                log_dir,
                Some(slot),
            )
        });
    }
    execute_trial_merge_at(
        root,
        round,
        task_id,
        audit_identity,
        storage_permit,
        orphan_context,
        plan,
        &commits,
        gate_refs,
        commands,
        log_dir,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_trial_merge_at(
    root: &Path,
    round: &str,
    task_id: &str,
    audit_identity: ledger::GateAuditIdentity<'_>,
    storage_permit: &crate::storage::StoragePermit,
    orphan_context: Option<(&str, &str)>,
    plan: &TrialMergePlan,
    commits: &[String],
    gate_refs: &[String],
    commands: &BTreeMap<String, binding::CommandSpec>,
    log_dir: &Path,
    slot: Option<&buildcache::TrialSlot>,
) -> Result<TrialMergeVerdict> {
    let scratch = PathBuf::from(&plan.scratch_worktree);
    if fs::symlink_metadata(&scratch).is_ok() {
        bail!("试合并 scratch 已存在，拒绝复用: {}", scratch.display());
    }
    fs::create_dir_all(scratch.parent().context("试合并 scratch 缺 parent")?)?;
    gitx::worktree_add_detached(root, &scratch, &plan.attempt_head)?;
    let mut guard = TrialWorktree {
        root,
        path: scratch.clone(),
        active: true,
    };

    let run = (|| -> Result<TrialMergeVerdict> {
        if gitx::rev_parse(&scratch, "HEAD")? != plan.attempt_head {
            bail!("试合并 detached worktree HEAD 与 attempt concrete SHA 不符");
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&scratch)
            .args([
                "merge",
                "--no-commit",
                "--no-ff",
                "--no-verify",
                &plan.main_head,
            ])
            .output()
            .context("启动试合并 git merge 失败")?;
        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let files = conflict_files(&scratch, &stdout, &stderr);
            if files.is_empty() {
                bail!(
                    "试合并 git merge 失败但未解析出冲突文件（exit={}）：{}{}",
                    output.status.code().unwrap_or(-1),
                    stdout.trim(),
                    stderr.trim()
                );
            }
            return Ok(TrialMergeVerdict::Conflict { files });
        }

        let mut cache_target = if let Some(slot) = slot {
            let cargo_program = buildcache::trial_cargo_program(gate_refs, commands)
                .context("trial slot selected without a uniform Cargo command")?;
            let config_digest = buildcache::build_config_digest(gate_refs, commands)?;
            let identity =
                buildcache::inspect_build_identity(&scratch, &cargo_program, config_digest)?;
            let hot_target = root.join("orch/target");
            let target = buildcache::prepare_trial_target(slot, &identity, Some(&hot_target))?;
            println!(
                "trial-cache slot={} generation={} target={} · {}",
                slot.index(),
                target.generation(),
                target.target_dir().display(),
                target.warm_preparation().render()
            );
            Some((identity, target))
        } else {
            None
        };

        if orphan_context.is_some_and(|(context_round, _)| context_round != round) {
            bail!("collect gate round scope 与 orphan context 不一致");
        }
        let subject_tree_sha = gate::capture_gate_subject_tree(root, &scratch)?;
        for (gate_index, gate_ref) in gate_refs.iter().enumerate() {
            let spec = commands
                .get(gate_ref)
                .with_context(|| format!("试合并绑定缺命令: {gate_ref}"))?;
            // The outer permit covers trial-worktree creation; the typed gate wrappers re-probe
            // immediately before every warm, uncached, and fallback child spawn.
            let gate_run_id = ulid::Ulid::new().to_string();
            let scoped_tag = gate::phase_scoped_log_tag(
                round,
                task_id,
                audit_identity.attempt_id().unwrap_or("pre-attempt"),
                gate::GatePhase::Trial,
                &gate_run_id,
            );
            let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
            let mut result = with_collect_orphan_watch(
                root,
                orphan_context,
                log_dir,
                &scoped_tag,
                gate_ref,
                || {
                    match cache_target.as_ref() {
                        Some((_, target)) => {
                            // `run_trial_gate` reaches the common `gate_log_sinks` production path; the
                            // warm assessment below therefore rereads one non-overwriting stdout/stderr
                            // log rather than the benchmark helper's separately concatenated buffers.
                            crate::storage::refresh_gate_permit(
                                root,
                                round,
                                audit_identity,
                                storage_permit,
                                &[
                                    scratch.clone(),
                                    log_dir.to_path_buf(),
                                    root.join("orch/target"),
                                    target.target_dir().to_path_buf(),
                                ],
                            )?;
                            gate::run_trial_gate_with_permit_and_identity(
                                storage_permit,
                                audit_identity,
                                gate_ref,
                                spec,
                                &scratch,
                                log_dir,
                                &scoped_tag,
                                target,
                            )
                        }
                        None => {
                            let expected_tag = gate::phase_scoped_log_tag(
                                round,
                                task_id,
                                audit_identity.attempt_id().unwrap_or("pre-attempt"),
                                gate::GatePhase::Trial,
                                &gate_run_id,
                            );
                            if scoped_tag != expected_tag {
                                bail!("trial gate tag 与 runtime gateRunId 不一致");
                            }
                            crate::storage::refresh_gate_permit(
                                root,
                                round,
                                audit_identity,
                                storage_permit,
                                &[
                                    scratch.clone(),
                                    log_dir.to_path_buf(),
                                    root.join("orch/target"),
                                ],
                            )?;
                            gate::run_gate_with_permit_and_identity(
                                storage_permit,
                                audit_identity,
                                gate_ref,
                                spec,
                                &scratch,
                                log_dir,
                                &scoped_tag,
                            )
                        }
                    }
                },
            )?;
            gate::record_gate_execution(
                root,
                round,
                audit_identity,
                GATE_EXECUTED_SCHEMA,
                gate::GatePhase::Trial,
                &gate_run_id,
                &subject_tree_sha,
                &result,
                &fingerprint,
            )?;

            // The first warm run is evidence, not a blind optimization.  If its log cannot prove
            // orch-core/orch-host rebuilt while registry dependencies stayed warm, abandon that
            // generation and re-run the gate against a fresh cold generation.  The warm target is
            // preserved for diagnosis; it is never cleaned or rebound in place.
            if gate_index == 0 {
                let assessment = cache_target
                    .as_ref()
                    .map(|(_, target)| {
                        target.assess_gate_log(result.exit_code, Path::new(&result.log_path))
                    })
                    .transpose()?;
                if let Some(assessment) = assessment {
                    if !matches!(assessment, buildcache::WarmStartAssessment::NotApplicable) {
                        println!("{}", assessment.render());
                    }
                    if assessment.requires_fallback() {
                        let slot = slot.context("warm fallback requires a held trial slot")?;
                        let identity = &cache_target
                            .as_ref()
                            .context("warm fallback missing build identity")?
                            .0;
                        let cold = buildcache::prepare_cold_fallback_target(
                            slot,
                            identity,
                            assessment.render(),
                        )?;
                        println!(
                            "trial-cache slot={} generation={} target={} · {}",
                            slot.index(),
                            cold.generation(),
                            cold.target_dir().display(),
                            cold.warm_preparation().render()
                        );
                        let gate_run_id = ulid::Ulid::new().to_string();
                        let scoped_tag = gate::phase_scoped_log_tag(
                            round,
                            task_id,
                            audit_identity.attempt_id().unwrap_or("pre-attempt"),
                            gate::GatePhase::Trial,
                            &gate_run_id,
                        );
                        let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
                        result = with_collect_orphan_watch(
                            root,
                            orphan_context,
                            log_dir,
                            &scoped_tag,
                            gate_ref,
                            || {
                                crate::storage::refresh_gate_permit(
                                    root,
                                    round,
                                    audit_identity,
                                    storage_permit,
                                    &[
                                        scratch.clone(),
                                        log_dir.to_path_buf(),
                                        root.join("orch/target"),
                                        cold.target_dir().to_path_buf(),
                                    ],
                                )?;
                                gate::run_trial_gate_with_permit_and_identity(
                                    storage_permit,
                                    audit_identity,
                                    gate_ref,
                                    spec,
                                    &scratch,
                                    log_dir,
                                    &scoped_tag,
                                    &cold,
                                )
                            },
                        )?;
                        gate::record_gate_execution(
                            root,
                            round,
                            audit_identity,
                            GATE_EXECUTED_SCHEMA,
                            gate::GatePhase::Trial,
                            &gate_run_id,
                            &subject_tree_sha,
                            &result,
                            &fingerprint,
                        )?;
                        let identity = identity.clone();
                        cache_target = Some((identity, cold));
                    }
                }
            }
            if result.exit_code != 0 {
                return Ok(TrialMergeVerdict::Red {
                    gate: gate_ref.clone(),
                    exit_code: result.exit_code,
                    interacting_commits: commits.to_vec(),
                    detail: gate_failure_detail(&result.log_path),
                });
            }
        }
        Ok(TrialMergeVerdict::Green)
    })();

    let cleanup = guard.cleanup();
    match (run, cleanup) {
        (Ok(verdict), Ok(())) => Ok(verdict),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error).context(format!(
            "试合并执行失败且 scratch 清理也失败: {cleanup_error:#}"
        )),
    }
}

fn run_trial_merge_if_needed(
    root: &Path,
    round: &str,
    task_id: &str,
    audit_identity: ledger::GateAuditIdentity<'_>,
    storage_permit: &crate::storage::StoragePermit,
    orphan_context: Option<(&str, &str)>,
    attempt_head: &str,
    base_sha: &str,
    current_main_sha: &str,
    gate_refs: &[String],
    commands: &BTreeMap<String, binding::CommandSpec>,
    log_dir: &Path,
) -> Result<Option<TrialMergeVerdict>> {
    if !trial_merge_needed(base_sha, current_main_sha) {
        return Ok(None);
    }
    if !gitx::is_ancestor(root, base_sha, current_main_sha)? {
        bail!(
            "current main {} 不是 attempt base {} 的后代，拒绝把 ref 倒退/改写当成同轮推进",
            gitx::short(current_main_sha),
            gitx::short(base_sha)
        );
    }
    let plan = trial_merge_plan(
        &root.to_string_lossy(),
        task_id,
        attempt_head,
        current_main_sha,
    )?;
    let commits = interacting_commits(root, base_sha, current_main_sha)?;
    execute_trial_merge(
        root,
        round,
        task_id,
        audit_identity,
        storage_permit,
        orphan_context,
        &plan,
        commits,
        gate_refs,
        commands,
        log_dir,
    )
    .map(Some)
}

/// Validate and collect a committed REPORT, then run the candidate and trial gates.
///
/// The caller must already have recorded `ReportObserved`. Every spawned gate receives a fresh
/// phase-scoped run identity and persists the same-shaped environment-bound observation.
/// Red and candidate tests use an independent Git snapshot whose private main matches the
/// fixed phase commit. Ambient Git routing and CARGO_TARGET_DIR overrides are rejected so
/// historical nested clones and the default target cannot silently select another source.
/// A red error preserves the snapshot without entering the candidate phase; the existing
/// oracle may already have restored detached HEAD. Successful collection also requires
/// explicit clean, identity-stable, quiescent snapshot removal. Unknown cleanup is an error.
pub fn check_and_gate(
    root: &Path,
    round: &str,
    c: &card::Card,
    branch: &str,
    report_rel: &str,
    expected_base: Option<&str>,
    worktree: &Path,
) -> Result<CollectOutcome> {
    reject_collect_source_redirects()?;
    let requested_task_id = c.meta.task_id.clone();
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取 collect attempt 账本失败: {}", ledger_path.display()))?;
    crate::attempt::reject_bad_lines(&ledger_read)?;
    let dispatch =
        crate::attempt::resolve_current_dispatch(&ledger_read.events, &requested_task_id, round)?;
    let attempt_id = dispatch
        .attempt_id
        .context("collect gate 缺 durable current attemptId")?;
    let policy_base_sha = dispatch
        .base_sha
        .clone()
        .or_else(|| expected_base.map(str::to_string))
        .context("collect lane resolution 缺 DispatchIssued policy baseSha")?;
    let audit_identity = ledger::GateAuditIdentity::Attempt {
        task_id: &c.meta.task_id,
        attempt_id: &attempt_id,
    };
    audit_identity.validate()?;

    // One outer permit precedes seed replay, trial-worktree creation, log creation and every gate
    // child spawned by collect. Holding it across the sequence prevents self-races between gates.
    let log_dir = root.join("coordination/runtime/logs");
    let _storage_permit = crate::storage::guard_gate_operation(
        root,
        round,
        audit_identity,
        &[
            worktree.to_path_buf(),
            log_dir.clone(),
            root.join("orch/target"),
            root.join(".cowork-temp"),
            root.join(".cowork-temp/trial-cache/slots"),
        ],
    )?;
    let mut m = match mech::check(root, c, branch, report_rel, expected_base) {
        Ok(m) => m,
        Err(e) => {
            let msg = e.to_string();
            let stage = infer_mech_stage(&msg);
            record_failure(root, round, &c.meta.task_id, stage, &msg)?;
            return Err(e);
        }
    };
    let tree_paths = match mech::tree_paths_at_head(root, branch) {
        Ok(paths) => paths,
        Err(e) => {
            let e = anyhow::anyhow!("REPORT 未 commit(E15): 无法读取 {branch} HEAD 树: {e}");
            let msg = e.to_string();
            record_failure(root, round, &c.meta.task_id, "report-committed", &msg)?;
            return Err(e);
        }
    };
    if !mech::report_committed(&tree_paths, report_rel) {
        let e = anyhow::anyhow!("REPORT 未 commit(E15): {report_rel} 不在 {branch} HEAD");
        let msg = e.to_string();
        record_failure(root, round, &c.meta.task_id, "report-committed", &msg)?;
        return Err(e);
    }
    m.notes
        .push("REPORT 已 commit ✅ 分支 HEAD 树含精确路径".into());
    if c.meta.schema_version == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
        && !m.seed_digests.is_empty()
    {
        let task_id = c.meta.task_id.clone();
        let seed_digests = m.seed_digests.clone();
        ledger::append_checked(root, round, move |events| {
            oracle::v3_seed_relocation_batch(events, round, &task_id, &seed_digests)
        })?;
    } else {
        for (target, digest) in &m.seed_digests {
            ledger::append(
                root,
                round,
                &[ledger::event(
                    "SeedRelocated",
                    "runtime:orch",
                    Some(&c.meta.task_id),
                    Some(round),
                    serde_json::json!({
                        "target": target,
                        "sha256": digest,
                        "cmp": "identical",
                    }),
                )],
            )?;
        }
    }
    println!("④ 机检通过: {}", m.notes.join("；"));

    // Keep the caller's immutable worktree for evidence; compile tests only
    // from a private ref namespace represented by an independent repository.
    let candidate_source = gitx::rev_parse(root, branch)?;
    let red_root_main = gitx::rev_parse(root, "main")?;
    let mut source_snapshot =
        CollectSourceSnapshot::new(root, &candidate_source, &_storage_permit)?;
    let source_path = source_snapshot.path.clone();
    let worktree = source_path.as_path();
    if !c.meta.seeds.is_empty() {
        let seed_source = gitx::commits_after(
            root,
            &gitx::merge_base(root, &red_root_main, branch)?,
            branch,
        )?
        .into_iter()
        .next()
        .context("collect source lacks first seed commit")?;
        source_snapshot.select(&seed_source)?;
    }

    // ④½ 机检级先红复跑（M2/E12 零模型兜底）：seed commit 处复跑测试门核对红计数
    // 复跑命令经 red_replay_gate 卡驱动解析（B32）
    let replay_gate =
        oracle::red_replay_gate(&c.meta.gates.fast).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("④½ 先红复跑: 复跑门={replay_gate}（卡 gates.fast[0]）");
    let replay_run_id = ulid::Ulid::new().to_string();
    let replay_tag = gate::phase_scoped_log_tag(
        round,
        &c.meta.task_id,
        &attempt_id,
        gate::GatePhase::RedReplay,
        &replay_run_id,
    );
    let replay_result = with_collect_orphan_watch(
        root,
        Some((round, &c.meta.task_id)),
        &log_dir,
        &replay_tag,
        &replay_gate,
        || {
            oracle::replay_seed_red(
                root,
                round,
                c,
                branch,
                worktree,
                &attempt_id,
                &_storage_permit,
                &replay_run_id,
                &replay_tag,
            )
        },
    );
    let replay_result = if gitx::rev_parse(root, "main")? != red_root_main {
        Err(anyhow::anyhow!(
            "root main changed during fixed seed replay; snapshot retained"
        ))
    } else {
        replay_result
    };
    let replay_result =
        finish_collect_red_phase(&mut source_snapshot, &candidate_source, replay_result);
    if let Err(e) = replay_result {
        let msg = e.to_string();
        let stage = if msg.contains("报数造假") || msg.contains("claim") {
            "red-replay-claim"
        } else {
            "red-replay"
        };
        record_failure(root, round, &c.meta.task_id, stage, &msg)?;
        return Err(e);
    }

    let candidate_oid = gitx::rev_parse(root, branch)?;
    let lane_plan = resolve_collect_lane_plan_at_candidate(
        root,
        round,
        c,
        &ledger_read.events,
        &attempt_id,
        &policy_base_sha,
        &candidate_oid,
    )?;
    if let Some(escalation) = lane_plan.escalation.as_ref() {
        let attempt_no = dispatch
            .attempt_no
            .context("GateLaneEscalated 缺 DispatchIssued attemptNo")?;
        append_gate_lane_escalation_once(
            root,
            round,
            &c.meta.task_id,
            &attempt_id,
            attempt_no,
            &policy_base_sha,
            escalation,
        )?;
        println!(
            "⑤ candidate lane 升级 fast: {} · digest={}",
            escalation.reason, escalation.resolved_command_digest
        );
    }
    let subject_tree_sha = gate::capture_gate_subject_tree(root, worktree)?;
    let mut gates = Vec::new();
    for command in &lane_plan.commands {
        let gref = &command.name;
        let spec = &command.spec;
        let before_gate_tree = gate::capture_gate_subject_tree(root, worktree)?;
        if before_gate_tree != subject_tree_sha {
            bail!(
                "collect gate {gref} 前 subject tree 漂移：expected={} observed={}",
                subject_tree_sha,
                before_gate_tree
            );
        }
        let gate_run_id = ulid::Ulid::new().to_string();
        let scoped_tag = gate::phase_scoped_log_tag(
            round,
            &c.meta.task_id,
            &attempt_id,
            gate::GatePhase::Collect,
            &gate_run_id,
        );
        let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
        let g = with_collect_orphan_watch(
            root,
            Some((round, &c.meta.task_id)),
            &log_dir,
            &scoped_tag,
            gref,
            || {
                crate::storage::refresh_gate_permit(
                    root,
                    round,
                    audit_identity,
                    &_storage_permit,
                    &[
                        worktree.to_path_buf(),
                        log_dir.to_path_buf(),
                        root.join("orch/target"),
                    ],
                )?;
                if lane_plan.candidate_narrow {
                    gate::run_candidate_gate_with_permit_and_identity(
                        &_storage_permit,
                        audit_identity,
                        gref,
                        spec,
                        worktree,
                        &log_dir,
                        &scoped_tag,
                    )
                } else {
                    gate::run_gate_with_permit_and_identity(
                        &_storage_permit,
                        audit_identity,
                        gref,
                        spec,
                        worktree,
                        &log_dir,
                        &scoped_tag,
                    )
                }
            },
        )?;
        let after_gate_tree = gate::capture_gate_subject_tree(root, worktree)?;
        if after_gate_tree != subject_tree_sha {
            bail!(
                "collect gate {gref} 执行期间 subject tree 漂移：expected={} observed={}",
                subject_tree_sha,
                after_gate_tree
            );
        }
        gate::record_gate_execution(
            root,
            round,
            audit_identity,
            GATE_EXECUTED_SCHEMA,
            gate::GatePhase::Collect,
            &gate_run_id,
            &subject_tree_sha,
            &g,
            &fingerprint,
        )?;
        println!("⑤ 门 {gref}: exit={} ({}ms)", g.exit_code, g.duration_ms);
        if g.exit_code != 0 {
            bail!("门 {gref} 红（exit {}），日志 {}", g.exit_code, g.log_path);
        }
        gates.push(g);
    }

    // ⑤½ 收取期试合并门（B161）：只有 main 相对本 attempt 基线推进过才付费。
    // attempt/main 均钉 concrete SHA，合并只发生在仓内 detached scratch；
    // 不 checkout main、不移动 main/task ref，所有返回路径都显式 remove scratch。
    if let Some(base_sha) = expected_base {
        let current_main = gitx::rev_parse(root, "main")?;
        let attempt_head = gitx::rev_parse(root, branch)?;
        let final_tree_active = final_tree_policy_active(
            root,
            round,
            &ledger_read.events,
            &c.meta.task_id,
            &attempt_id,
            &policy_base_sha,
        )?;
        let trial_refs = if final_tree_active {
            println!(
                "⑤½ final-tree-v1 active: trial 仅执行 conflict/path preflight，workspace full 延后到 seal"
            );
            &[][..]
        } else {
            lane_plan.trial_refs.as_slice()
        };
        if let Some(verdict) = run_trial_merge_if_needed(
            root,
            round,
            &c.meta.task_id,
            audit_identity,
            &_storage_permit,
            Some((round, &c.meta.task_id)),
            &attempt_head,
            base_sha,
            &current_main,
            trial_refs,
            &lane_plan.committed_binding.commands,
            &log_dir,
        )? {
            println!("⑤½ {}", verdict.render());
            if !verdict.is_ok() {
                bail!("{}", verdict.render());
            }
        }
    }
    source_snapshot.close()?;
    Ok(CollectOutcome {
        mech_notes: m.notes,
        gates,
    })
}

/// 按 mech::check 错误信息推断机检环节（stage）。失败也落账（E8 补充），
/// stage 取对应环节名 domain/frozen/shape/seed-bytes。
fn infer_mech_stage(msg: &str) -> &'static str {
    if msg.contains("冻结") || msg.contains("frozen") {
        "frozen"
    } else if msg.contains("字节") || msg.contains("SHA") || msg.contains("种子") {
        "seed-bytes"
    } else if msg.contains("首 commit")
        || msg.contains("无提交")
        || msg.contains("形状")
        || msg.contains("upstream")
    {
        "shape"
    } else {
        "domain"
    }
}

/// 机检失败落账：先 append MechCheckFailed 事件再由调用方 bail。
fn record_failure(
    root: &Path,
    round: &str,
    task_id: &str,
    stage: &str,
    reason: &str,
) -> Result<()> {
    ledger::append(
        root,
        round,
        &[mech::failure_event(task_id, round, stage, reason)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_all(root: &Path, message: &str) -> String {
        git(root, &["add", "-A"]);
        git(
            root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "commit",
                "-m",
                message,
            ],
        );
        git(root, &["rev-parse", "HEAD"])
    }

    fn init_repo(tag: &str) -> (PathBuf, String) {
        let root = crate::util::test_scratch_dir(tag);
        git(&root, &["init", "-b", "main"]);
        fs::write(
            root.join(".gitignore"),
            ".cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(root.join("collision.txt"), "base\n").unwrap();
        let base = commit_all(&root, "base");
        (root, base)
    }

    fn gate_commands(command: &str) -> BTreeMap<String, binding::CommandSpec> {
        BTreeMap::from([(
            "testFast".to_string(),
            binding::CommandSpec {
                argv: vec!["sh".into(), "-c".into(), command.into()],
                timeout_seconds: 30,
                trial_timeout_seconds: None,
                approval: None,
            },
        )])
    }

    fn fixture_storage_permit(root: &Path) -> crate::storage::StoragePermit {
        crate::storage::fixture_gate_permit(
            root,
            &[
                root.to_path_buf(),
                root.join("coordination/runtime/logs"),
                root.join("orch/target"),
                root.join(".cowork-temp"),
            ],
        )
    }

    fn snapshot_test_history(tag: &str) -> (PathBuf, String, String, String, Vec<u8>) {
        let (root, _) = init_repo(tag);
        let hook = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .unwrap()
                .join(".githooks/reference-transaction"),
        )
        .unwrap();
        fs::create_dir_all(root.join(".githooks")).unwrap();
        fs::write(root.join(".githooks/reference-transaction"), &hook).unwrap();
        fs::write(root.join("subject.txt"), "seed\n").unwrap();
        let seed = commit_all(&root, "immutable seed source");
        fs::write(root.join("subject.txt"), "candidate\n").unwrap();
        let candidate = commit_all(&root, "candidate source");
        let mut newer = hook.clone();
        newer.extend_from_slice(b"\n# later main version\n");
        fs::write(root.join(".githooks/reference-transaction"), newer).unwrap();
        let main = commit_all(&root, "main moved independently");
        (root, seed, candidate, main, hook)
    }

    fn assert_snapshot_nested_source(root: &Path, snapshot: &Path, expected: &str, hook: &[u8]) {
        let nested = root
            .join(".cowork-temp")
            .join(crate::util::unique_scratch_name("nested-fixture"));
        git(
            root,
            &[
                "clone",
                "-q",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                snapshot.to_str().unwrap(),
                nested.to_str().unwrap(),
            ],
        );
        // This is the unchanged historical fixture contract that exposed the bug.
        git(&nested, &["checkout", "-q", "-B", "main", "origin/main"]);
        crate::hooks::ensure_main_guard(&nested)
            .expect("fixture hook must match the code compiled from its fixed source");
        assert_eq!(git(&nested, &["rev-parse", "HEAD"]), expected);
        assert_eq!(
            fs::read(nested.join(".githooks/reference-transaction")).unwrap(),
            hook
        );
    }

    #[test]
    fn collect_snapshot_candidate_nested_clone_uses_fixed_source() {
        let (root, _, candidate, main, hook) = snapshot_test_history("collect-source-candidate");
        let before_refs = git(&root, &["show-ref", "--heads"]);
        let permit = fixture_storage_permit(&root);
        let mut snapshot = CollectSourceSnapshot::new(&root, &candidate, &permit).unwrap();
        assert_snapshot_nested_source(&root, &snapshot.path, &candidate, &hook);
        assert!(snapshot.path.join(".git").is_dir());
        assert_ne!(
            gitx::canonical_worktree_common_dir(&root).unwrap(),
            gitx::canonical_worktree_common_dir(&snapshot.path).unwrap()
        );
        let path = snapshot.path.clone();
        snapshot.close().unwrap();
        drop(snapshot);
        assert!(!path.exists());
        assert_eq!(git(&root, &["show-ref", "--heads"]), before_refs);
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main);
    }

    #[test]
    fn collect_snapshot_seed_and_candidate_refs_are_private_and_restored() {
        let (root, seed, candidate, main, hook) = snapshot_test_history("collect-source-seed");
        let before_refs = git(&root, &["show-ref", "--heads"]);
        let permit = fixture_storage_permit(&root);
        let mut snapshot = CollectSourceSnapshot::new(&root, &candidate, &permit).unwrap();
        snapshot.select(&seed).unwrap();
        assert_snapshot_nested_source(&root, &snapshot.path, &seed, &hook);
        // Preserve the existing oracle's own detached-HEAD restoration before
        // restoring the private main ref and running the candidate gates.
        gitx::checkout(&snapshot.path, &candidate).unwrap();
        snapshot.select(&candidate).unwrap();
        assert_snapshot_nested_source(&root, &snapshot.path, &candidate, &hook);
        snapshot.close().unwrap();
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main);
        assert_eq!(git(&root, &["show-ref", "--heads"]), before_refs);
    }

    #[test]
    fn collect_snapshot_error_keeps_oracle_transition_without_candidate_phase() {
        let (root, seed, candidate, main, _) = snapshot_test_history("collect-source-error");
        let permit = fixture_storage_permit(&root);
        let mut source = CollectSourceSnapshot::new(&root, &candidate, &permit).unwrap();
        source.select(&seed).unwrap();
        gitx::checkout(&source.path, &candidate).unwrap();
        let failure = finish_collect_red_phase(
            &mut source,
            &candidate,
            Err(anyhow::anyhow!("cleanup unknown")),
        );
        assert!(failure.unwrap_err().to_string().contains("cleanup unknown"));
        assert_eq!(git(&source.path, &["rev-parse", "main"]), seed);
        assert_eq!(git(&source.path, &["rev-parse", "HEAD"]), candidate);
        let path = source.path.clone();
        drop(source);
        assert!(path.exists(), "unknown cleanup must preserve the source");
        assert_eq!(git(&root, &["rev-parse", "main"]), main);
    }

    #[test]
    fn collect_snapshot_ref_and_external_object_drift_are_refused() {
        let (root, seed, candidate, main, _) = snapshot_test_history("collect-source-drift");
        let permit = fixture_storage_permit(&root);
        let mut source = CollectSourceSnapshot::new(&root, &candidate, &permit).unwrap();
        git(
            &source.path,
            &["update-ref", "refs/heads/main", &main, &candidate],
        );
        assert!(source.select(&seed).is_err());
        assert!(source.close().is_err());
        git(
            &source.path,
            &["update-ref", "refs/heads/main", &candidate, &main],
        );
        let alternate = source.path.join(".git/objects/info/alternates");
        fs::create_dir_all(alternate.parent().unwrap()).unwrap();
        fs::write(
            &alternate,
            format!("{}\n", root.join(".git/objects").display()),
        )
        .unwrap();
        assert!(source.select(&seed).is_err());
        assert!(source.close().is_err());
        fs::remove_file(alternate).unwrap();
        source.close().unwrap();
        assert_eq!(git(&root, &["rev-parse", "main"]), main);
    }

    #[test]
    fn collect_snapshot_replacement_is_preserved() {
        let (root, _, candidate, main, _) = snapshot_test_history("collect-source-replacement");
        let permit = fixture_storage_permit(&root);
        let mut source = CollectSourceSnapshot::new(&root, &candidate, &permit).unwrap();
        let path = source.path.clone();
        let retained = path.with_extension("original");
        fs::rename(&path, &retained).unwrap();
        git(
            &root,
            &[
                "clone",
                "-q",
                "--no-local",
                retained.to_str().unwrap(),
                path.to_str().unwrap(),
            ],
        );
        git(&path, &["checkout", "-q", "--detach", &candidate]);
        assert!(source.select(&candidate).is_err());
        assert!(source.close().is_err());
        drop(source);
        assert_eq!(
            fs::read_to_string(path.join("subject.txt")).unwrap(),
            "candidate\n"
        );
        assert!(retained.join(".git").exists());
        assert_eq!(git(&root, &["rev-parse", "main"]), main);
    }

    #[test]
    fn collect_snapshot_external_common_dir_cannot_move_root_main() {
        let (root, seed, _, main, _) = snapshot_test_history("collect-source-common");
        let permit = fixture_storage_permit(&root);
        let mut source = CollectSourceSnapshot::new(&root, &main, &permit).unwrap();
        fs::write(
            source.path.join(".git/commondir"),
            format!("{}\n", root.join(".git").display()),
        )
        .unwrap();
        assert!(source.select(&seed).is_err());
        assert_eq!(
            git(&root, &["rev-parse", "main"]),
            main,
            "a refused phase must never change the real root"
        );
        assert!(source.close().is_err());
    }

    struct SnapshotTestChild(std::process::Child);
    impl Drop for SnapshotTestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn collect_snapshot_open_process_blocks_cleanup_without_stopping_it() {
        let (root, _, candidate, _, _) = snapshot_test_history("collect-source-busy");
        let permit = fixture_storage_permit(&root);
        let mut source = CollectSourceSnapshot::new(&root, &candidate, &permit).unwrap();
        let mut child = SnapshotTestChild(
            Command::new("/bin/sleep")
                .arg("30")
                .current_dir(&source.path)
                .spawn()
                .unwrap(),
        );
        assert!(source.close().is_err());
        assert!(child.0.try_wait().unwrap().is_none());
        drop(child);
        source.close().unwrap();
    }

    #[test]
    fn collect_snapshot_redirect_context_child() {
        let Ok(key) = std::env::var("ORCH_SNAPSHOT_CONTEXT_CHILD") else {
            return;
        };
        let error = reject_collect_source_redirects().unwrap_err().to_string();
        assert!(error.contains(&key), "{error}");
    }

    #[test]
    fn collect_snapshot_outer_redirects_are_rejected_in_isolated_processes() {
        let keys = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "CARGO_TARGET_DIR",
        ];
        for key in keys {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "collect::tests::collect_snapshot_redirect_context_child",
                "--nocapture",
            ]);
            for removed in keys {
                command.env_remove(removed);
            }
            let result = command
                .env("ORCH_SNAPSHOT_CONTEXT_CHILD", key)
                .env(key, "/unusable/redirect")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{key}: {}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }

    #[test]
    fn signed_legacy_shape_slot_maps_only_to_the_descriptor_closure_slot() {
        let binding: binding::Binding = serde_yaml::from_str(
            "gates:\n  candidate: [seedTargets, shapeTests, check]\n  merge: [full]\n  fast: [full]\n",
        )
        .unwrap();
        assert_eq!(
            normalize_candidate_refs(&binding),
            (
                vec![
                    "seedTargets".to_string(),
                    "sourceReaderClosure".to_string(),
                    "check".to_string(),
                ],
                None,
            )
        );

        let ambiguous: binding::Binding = serde_yaml::from_str(
            "gates:\n  candidate: [shapeTests, sourceReaderClosure]\n  merge: [full]\n  fast: [full]\n",
        )
        .unwrap();
        assert!(normalize_candidate_refs(&ambiguous).1.is_some());
    }

    #[test]
    fn seed_selector_accepts_only_the_cards_integration_test_shape() {
        assert_eq!(
            integration_selector("orch/crates/orch-host/tests/gate_lane_contract.rs").unwrap(),
            ("orch-host".to_string(), "gate_lane_contract".to_string())
        );
        assert!(integration_selector("orch/crates/orch-host/src/gate.rs").is_err());
        assert!(integration_selector("orch/crates/orch-host/tests/support/mod.rs").is_err());
        assert!(integration_selector("/orch/crates/orch-host/tests/escape.rs").is_err());
        assert!(integration_selector("./orch/crates/orch-host/tests/escape.rs").is_err());
    }

    #[test]
    fn unknown_candidate_edge_appends_one_exact_typed_escalation() {
        let root = crate::util::test_scratch_dir("b306-lane-escalation");
        let round = "r99";
        let task = "B306";
        let attempt = "B306-A0001";
        let base = "a".repeat(40);
        fs::create_dir_all(root.join(format!("coordination/rounds/{round}"))).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/locks")).unwrap();
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{round}\n"),
        )
        .unwrap();
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(task),
            Some(round),
            serde_json::json!({
                "attemptId": attempt,
                "attemptNo": 1,
                "baseSha": base,
                "goPath": "coordination/rounds/r99/dispatch/executor/GO-B306-A0001.md"
            }),
        );
        let line = format!("{}\n", serde_json::to_string(&dispatch).unwrap());
        fs::write(
            root.join(format!("coordination/rounds/{round}/events.jsonl")),
            &line,
        )
        .unwrap();
        fs::write(
            root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl")),
            &line,
        )
        .unwrap();

        let escalation = GateLaneEscalation {
            reason: "unregistered dynamic reader".to_string(),
            resolved_command_digest: "b".repeat(64),
        };
        append_gate_lane_escalation_once(&root, round, task, attempt, 1, &base, &escalation)
            .unwrap();
        append_gate_lane_escalation_once(&root, round, task, attempt, 1, &base, &escalation)
            .unwrap();

        let read =
            orch_core::read_ledger(&root.join(format!("coordination/rounds/{round}/events.jsonl")))
                .unwrap();
        let events = read
            .events
            .iter()
            .filter(|event| event.kind == "GateLaneEscalated")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert!(ledger::canonical_runtime_event_v1(events[0], round).unwrap());
        let payload = events[0].payload.as_ref().unwrap().as_object().unwrap();
        assert_eq!(
            payload.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "schemaVersion",
                "attemptId",
                "attemptNo",
                "fromLane",
                "toLane",
                "reason",
                "policyBaseSha",
                "resolvedCommandDigest",
            ])
        );
    }

    #[test]
    fn long_gate_log_keeps_an_early_failed_test_name() {
        let root = crate::util::test_scratch_dir("collect-trial-log-detail");
        let log = root.join("red.log");
        let mut bytes = b"test semantic_interaction ... FAILED\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', 20 * 1024));
        fs::write(&log, bytes).unwrap();
        let detail = gate_failure_detail(&log.to_string_lossy());
        assert!(detail.contains("semantic_interaction"));
        assert!(detail.contains("日志尾部"));
    }

    #[test]
    fn r53_semantic_interaction_is_red_but_reverting_the_other_card_is_green() {
        let (root, base) = init_repo("collect-trial-semantic");
        fs::create_dir_all(root.join("coordination/rounds/r-test")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r-test\n").unwrap();
        fs::write(root.join("coordination/rounds/r-test/events.jsonl"), "").unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/r-test.jsonl"),
            "",
        )
        .unwrap();
        let commands = gate_commands(
            "if [ -f require-entrypoints ] && [ -f fixture-card.md ] && \
             ! grep -q '^entryPoints:' fixture-card.md; then \
             echo 'task B900: entryPoints 不得为空' >&2; exit 101; fi",
        );
        let gates = vec!["testFast".to_string()];
        let log_dir = root.join("coordination/runtime/logs");
        git(&root, &["switch", "-c", "task/B-fixture", &base]);
        fs::write(root.join("fixture-card.md"), "taskId: B900\nwriteSet: []\n").unwrap();
        let attempt = commit_all(&root, "B card adds a legacy fixture");
        let b_alone = gate::run_gate(
            "testFast",
            commands.get("testFast").unwrap(),
            &root,
            &log_dir,
            "B-alone",
        )
        .unwrap();
        assert_eq!(
            b_alone.exit_code, 0,
            "B branch's legacy fixture is valid under the old schema"
        );
        git(&root, &["switch", "main"]);
        fs::write(root.join("require-entrypoints"), "required\n").unwrap();
        let moved_main = commit_all(&root, "A card makes entryPoints mandatory");
        let a_alone = gate::run_gate(
            "testFast",
            commands.get("testFast").unwrap(),
            &root,
            &log_dir,
            "A-alone",
        )
        .unwrap();
        assert_eq!(
            a_alone.exit_code, 0,
            "A branch has no legacy fixture and remains green on its own"
        );
        let before_main = git(&root, &["rev-parse", "main"]);
        let before_task = git(&root, &["rev-parse", "task/B-fixture"]);
        let storage_permit = fixture_storage_permit(&root);

        let red = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &attempt,
            &base,
            &moved_main,
            &gates,
            &commands,
            &log_dir,
        )
        .unwrap()
        .expect("main moved, trial must run");
        let message = red.render();
        assert!(!red.is_ok());
        assert!(message.contains("testFast"));
        assert!(message.contains(gitx::short(&moved_main)));
        assert!(message.contains("entryPoints"));
        assert_eq!(git(&root, &["rev-parse", "main"]), before_main);
        assert_eq!(git(&root, &["rev-parse", "task/B-fixture"]), before_task);
        assert!(
            fs::read_dir(root.join(".cowork-temp"))
                .unwrap()
                .next()
                .is_none(),
            "red path must clean its scratch worktree"
        );

        fs::remove_file(root.join("require-entrypoints")).unwrap();
        let reverted_main = commit_all(&root, "remove A card's mandatory-field change");
        let green = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &attempt,
            &base,
            &reverted_main,
            &gates,
            &commands,
            &log_dir,
        )
        .unwrap()
        .expect("main still moved, trial must run");
        assert_eq!(green, TrialMergeVerdict::Green);
        assert_eq!(git(&root, &["rev-parse", "task/B-fixture"]), before_task);
        assert!(
            fs::read_dir(root.join(".cowork-temp"))
                .unwrap()
                .next()
                .is_none(),
            "green path must clean its scratch worktree"
        );
    }

    #[test]
    fn unchanged_main_skips_worktree_and_gate_entirely() {
        let (root, base) = init_repo("collect-trial-skip");
        let sentinel = root.join("gate-ran");
        let commands = gate_commands(&format!("touch '{}'", sentinel.display()));
        let storage_permit = fixture_storage_permit(&root);
        let verdict = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &base,
            &base,
            &base,
            &["testFast".to_string()],
            &commands,
            &root.join("coordination/runtime/logs"),
        )
        .unwrap();
        assert_eq!(verdict, None);
        assert!(
            !sentinel.exists(),
            "unchanged main must not run a second gate"
        );
        assert!(
            !root.join(".cowork-temp").exists(),
            "unchanged main must not create a worktree parent"
        );
    }

    #[test]
    fn conflict_refuses_with_file_and_cleans_without_moving_refs() {
        let (root, base) = init_repo("collect-trial-conflict");
        git(&root, &["switch", "-c", "task/B-conflict", &base]);
        fs::write(root.join("collision.txt"), "task\n").unwrap();
        let attempt = commit_all(&root, "task changes collision");
        git(&root, &["switch", "main"]);
        fs::write(root.join("collision.txt"), "main\n").unwrap();
        let moved_main = commit_all(&root, "main changes collision");
        let before_main = git(&root, &["rev-parse", "main"]);
        let before_task = git(&root, &["rev-parse", "task/B-conflict"]);
        let storage_permit = fixture_storage_permit(&root);

        let verdict = run_trial_merge_if_needed(
            &root,
            "r-test",
            "B900",
            ledger::GateAuditIdentity::Attempt {
                task_id: "B900",
                attempt_id: "B900-A0001",
            },
            &storage_permit,
            None,
            &attempt,
            &base,
            &moved_main,
            &["testFast".to_string()],
            &gate_commands("exit 0"),
            &root.join("coordination/runtime/logs"),
        )
        .unwrap()
        .expect("main moved, trial must run");
        assert_eq!(
            verdict,
            TrialMergeVerdict::Conflict {
                files: vec!["collision.txt".into()]
            }
        );
        assert_eq!(git(&root, &["rev-parse", "main"]), before_main);
        assert_eq!(git(&root, &["rev-parse", "task/B-conflict"]), before_task);
        assert!(
            fs::read_dir(root.join(".cowork-temp"))
                .unwrap()
                .next()
                .is_none(),
            "conflict path must clean its MERGING scratch worktree"
        );
    }

    #[test]
    fn collect_boundary_scopes_watcher_and_runner_artifacts_once() {
        let root = crate::util::test_scratch_dir("b272-collect-scoped-gate");
        let log_dir = root.join("coordination/runtime/logs");
        fs::create_dir_all(&log_dir).unwrap();
        let commands = gate_commands(": > \"$ORCH_GATE_FIXTURE_REGISTRY\"; echo collect-green");
        let result = with_round_scoped_collect_gate(
            &root,
            None,
            "r71",
            &log_dir,
            "B272-collect",
            "testFast",
            |scoped_tag| {
                gate::run_gate(
                    "testFast",
                    commands.get("testFast").unwrap(),
                    &root,
                    &log_dir,
                    scoped_tag,
                )
            },
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);

        let stem = "B272-collect-round-r71-gate-testFast";
        for extension in ["log", "hb", "orphans", "fixtures"] {
            assert!(
                log_dir.join(format!("{stem}.{extension}")).is_file(),
                "collect watcher/runner sibling missing: {extension}"
            );
        }
        assert!(!log_dir.join("B272-collect-gate-testFast.log").exists());
    }
}
