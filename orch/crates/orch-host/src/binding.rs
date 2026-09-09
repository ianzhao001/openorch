//! PROJECT-BINDING.yaml 薄封装（design/02 §5）。只解析本切片需要的字段，未知字段全容忍。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

fn closed_mapping<'a>(
    value: &'a serde_yaml::Value,
    label: &str,
    expected: &[&str],
) -> Result<&'a serde_yaml::Mapping> {
    let mapping = value
        .as_mapping()
        .with_context(|| format!("{label} 必须是 mapping"))?;
    let mut actual = BTreeSet::new();
    for key in mapping.keys() {
        let key = key
            .as_str()
            .with_context(|| format!("{label} 含非字符串 key"))?;
        actual.insert(key);
    }
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected {
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        let unknown = actual.difference(&expected).copied().collect::<Vec<_>>();
        bail!("{label} key set 非 exact：missing={missing:?} unknown={unknown:?}");
    }
    Ok(mapping)
}

fn mapping_value<'a>(
    mapping: &'a serde_yaml::Mapping,
    key: &str,
    label: &str,
) -> Result<&'a serde_yaml::Value> {
    mapping
        .get(&serde_yaml::Value::String(key.to_string()))
        .with_context(|| format!("{label} 缺 {key}"))
}

fn exact_string<'a>(
    mapping: &'a serde_yaml::Mapping,
    key: &str,
    label: &str,
) -> Result<&'a str> {
    let value = mapping_value(mapping, key, label)?
        .as_str()
        .with_context(|| format!("{label}.{key} 必须是 string"))?;
    if value.trim().is_empty() || value.trim() != value {
        bail!("{label}.{key} 必须是 non-blank exact string");
    }
    Ok(value)
}

fn exact_bool(mapping: &serde_yaml::Mapping, key: &str, label: &str) -> Result<bool> {
    mapping_value(mapping, key, label)?
        .as_bool()
        .with_context(|| format!("{label}.{key} 必须是 bool"))
}

fn positive_u64(mapping: &serde_yaml::Mapping, key: &str, label: &str) -> Result<u64> {
    mapping_value(mapping, key, label)?
        .as_u64()
        .filter(|value| *value > 0)
        .with_context(|| format!("{label}.{key} 必须是正整数"))
}

fn string_sequence<'a>(
    mapping: &'a serde_yaml::Mapping,
    key: &str,
    label: &str,
    allow_empty: bool,
) -> Result<Vec<&'a str>> {
    let sequence = mapping_value(mapping, key, label)?
        .as_sequence()
        .with_context(|| format!("{label}.{key} 必须是 sequence"))?;
    if !allow_empty && sequence.is_empty() {
        bail!("{label}.{key} 不得为空");
    }
    let mut values = Vec::with_capacity(sequence.len());
    let mut seen = BTreeSet::new();
    for value in sequence {
        let value = value
            .as_str()
            .with_context(|| format!("{label}.{key} 元素必须是 string"))?;
        if value.trim().is_empty() || value.trim() != value || !seen.insert(value) {
            bail!("{label}.{key} 元素必须 non-blank、exact 且唯一");
        }
        values.push(value);
    }
    Ok(values)
}

/// Validate the exact schema 3 binding surface before it can enter an IR
/// digest. Legacy binding parsing remains intentionally tolerant.
pub fn validate_v3_binding_shape(bytes: &[u8]) -> Result<()> {
    let raw: serde_yaml::Value =
        serde_yaml::from_slice(bytes).context("schema 3 PROJECT-BINDING 非 YAML")?;
    let root = closed_mapping(
        &raw,
        "PROJECT-BINDING",
        &[
            "apiVersion",
            "kind",
            "metadata",
            "project",
            "workspace",
            "commands",
            "gates",
            "scope",
            "git",
            "verification",
            "oracle",
            "data",
            "knownFailures",
        ],
    )?;
    if exact_string(root, "apiVersion", "PROJECT-BINDING")? != "orch/v1alpha1"
        || exact_string(root, "kind", "PROJECT-BINDING")? != "ProjectBinding"
    {
        bail!("schema 3 PROJECT-BINDING apiVersion/kind 非 canonical");
    }
    for (key, expected) in [
        ("metadata", &["name", "bindingRevision"][..]),
        (
            "project",
            &["root", "primaryBranch", "sourceOfTruth", "ecosystems"][..],
        ),
        (
            "workspace",
            &[
                "defaultIsolation",
                "worktreeRoot",
                "branchPattern",
                "generatedPaths",
            ][..],
        ),
        ("gates", &["candidate", "merge", "fast"][..]),
        ("scope", &["protectedPaths", "publicEntryPoints"][..]),
        (
            "git",
            &[
                "implementerMayCommit",
                "implementerMayMerge",
                "pushPolicy",
                "mergePolicy",
            ][..],
        ),
        ("verification", &["contractModes"][..]),
        ("oracle", &["dialect"][..]),
        ("data", &["secretsPolicy", "forbiddenArtifactPatterns"][..]),
    ] {
        closed_mapping(mapping_value(root, key, "PROJECT-BINDING")?, key, expected)?;
    }

    let metadata = mapping_value(root, "metadata", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    exact_string(metadata, "name", "metadata")?;
    positive_u64(metadata, "bindingRevision", "metadata")?;
    let project = mapping_value(root, "project", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    if exact_string(project, "root", "project")? != "."
        || exact_string(project, "primaryBranch", "project")? != "main"
    {
        bail!("project.root/primaryBranch 非 canonical");
    }
    string_sequence(project, "sourceOfTruth", "project", false)?;
    string_sequence(project, "ecosystems", "project", false)?;
    let workspace = mapping_value(root, "workspace", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    if exact_string(workspace, "defaultIsolation", "workspace")? != "git-worktree" {
        bail!("workspace.defaultIsolation 必须为 git-worktree");
    }
    exact_string(workspace, "worktreeRoot", "workspace")?;
    exact_string(workspace, "branchPattern", "workspace")?;
    string_sequence(workspace, "generatedPaths", "workspace", true)?;

    let commands = closed_mapping(
        mapping_value(root, "commands", "PROJECT-BINDING")?,
        "commands",
        &[
            "testFast",
            "testExclusive",
            "check",
            "checkDefault",
            "buildDefault",
            "buildSelfhost",
            "seedTargets",
        ],
    )?;
    for name in [
        "testFast",
        "testExclusive",
        "check",
        "checkDefault",
        "buildDefault",
        "buildSelfhost",
    ] {
        let command = closed_mapping(
            mapping_value(commands, name, "commands")?,
            &format!("commands.{name}"),
            &["argv", "timeoutSeconds"],
        )?;
        string_sequence(command, "argv", &format!("commands.{name}"), false)?;
        positive_u64(command, "timeoutSeconds", &format!("commands.{name}"))?;
    }
    let seed = closed_mapping(
        mapping_value(commands, "seedTargets", "commands")?,
        "commands.seedTargets",
        &["argv", "seedTargetArgs", "timeoutSeconds"],
    )?;
    if mapping_value(seed, "seedTargetArgs", "commands.seedTargets")?.as_bool() != Some(true) {
        bail!("commands.seedTargets.seedTargetArgs 必须精确为 true");
    }
    string_sequence(seed, "argv", "commands.seedTargets", false)?;
    positive_u64(seed, "timeoutSeconds", "commands.seedTargets")?;

    let gates = mapping_value(root, "gates", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    let known_commands = commands
        .keys()
        .map(|key| key.as_str().expect("command key checked"))
        .collect::<BTreeSet<_>>();
    for lane in ["candidate", "merge", "fast"] {
        for command in string_sequence(gates, lane, "gates", false)? {
            if !known_commands.contains(command) {
                bail!("gates.{lane} 引用未知 command: {command}");
            }
        }
    }
    let scope = mapping_value(root, "scope", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    string_sequence(scope, "protectedPaths", "scope", false)?;
    string_sequence(scope, "publicEntryPoints", "scope", true)?;
    let git = mapping_value(root, "git", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    if !exact_bool(git, "implementerMayCommit", "git")?
        || exact_bool(git, "implementerMayMerge", "git")?
    {
        bail!("schema 3 git implementerMayCommit/implementerMayMerge 必须为 true/false");
    }
    if exact_string(git, "pushPolicy", "git")? != "forbidden" {
        bail!("git.pushPolicy 必须为 forbidden");
    }
    if exact_string(git, "mergePolicy", "git")? != "ff-only-else-no-ff" {
        bail!("git.mergePolicy 必须为 ff-only-else-no-ff");
    }
    let verification = mapping_value(root, "verification", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    let contract_modes = string_sequence(
        verification,
        "contractModes",
        "verification",
        false,
    )?;
    if contract_modes != ["seeded-red", "verify-only"] {
        bail!("verification.contractModes 非 canonical schema 3 集合/顺序");
    }
    let oracle = mapping_value(root, "oracle", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    if exact_string(oracle, "dialect", "oracle")? != "cargo" {
        bail!("oracle.dialect 必须为 cargo");
    }
    let data = mapping_value(root, "data", "PROJECT-BINDING")?
        .as_mapping()
        .expect("closed mapping checked");
    exact_string(data, "secretsPolicy", "data")?;
    string_sequence(data, "forbiddenArtifactPatterns", "data", false)?;
    if !mapping_value(root, "knownFailures", "PROJECT-BINDING")?.is_sequence() {
        bail!("knownFailures 必须是 sequence");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct Binding {
    /// Named gate commands available to runtime gate resolution.
    #[serde(default)]
    pub commands: BTreeMap<String, CommandSpec>,
    /// Signed candidate/merge/fast lane declarations. An all-empty value is
    /// retained only for bindings created before lane support existed.
    #[serde(default)]
    pub gates: BindingGates,
    #[serde(default)]
    pub workspace: Workspace,
    #[serde(default)]
    pub oracle: Oracle,
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default)]
    pub git: Git,
}

/// Signed command-reference lists for the three V1 gate-strength lanes.
///
/// The nested mapping is closed so a misspelled lane cannot remain signed but
/// inert. Empty lists preserve parsing compatibility for historical bindings;
/// a V1 lane consumer still rejects an empty selected or upgrade lane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BindingGates {
    /// Narrow commands intended for candidate collection.
    pub candidate: Vec<String>,
    /// Full commands required at merge-strength boundaries.
    pub merge: Vec<String>,
    /// Compatibility/full-strength commands used for fail-closed upgrades.
    pub fast: Vec<String>,
    candidate_declared: bool,
    merge_declared: bool,
    fast_declared: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingGatesWire {
    #[serde(default)]
    candidate: BindingGateArm,
    #[serde(default)]
    merge: BindingGateArm,
    #[serde(default)]
    fast: BindingGateArm,
}

#[derive(Default)]
struct BindingGateArm {
    values: Vec<String>,
    declared: bool,
}

impl<'de> Deserialize<'de> for BindingGateArm {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserializing directly as Vec intentionally rejects explicit null;
        // only serde(default) on an absent mapping key produces declared=false.
        Ok(Self {
            values: Vec::<String>::deserialize(deserializer)?,
            declared: true,
        })
    }
}

impl<'de> Deserialize<'de> for BindingGates {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = BindingGatesWire::deserialize(deserializer)?;
        Ok(Self {
            candidate_declared: wire.candidate.declared,
            merge_declared: wire.merge.declared,
            fast_declared: wire.fast.declared,
            candidate: wire.candidate.values,
            merge: wire.merge.values,
            fast: wire.fast.values,
        })
    }
}

impl BindingGates {
    /// Whether `candidate` was explicitly present rather than legacy-absent.
    pub fn candidate_declared(&self) -> bool {
        self.candidate_declared
    }

    /// Whether `merge` was explicitly present rather than legacy-absent.
    pub fn merge_declared(&self) -> bool {
        self.merge_declared
    }

    /// Whether `fast` was explicitly present rather than legacy-absent.
    pub fn fast_declared(&self) -> bool {
        self.fast_declared
    }
}

#[derive(Debug, Deserialize)]
pub struct CommandSpec {
    /// Exact argument vector executed without a shell.
    pub argv: Vec<String>,
    #[serde(rename = "timeoutSeconds", default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(rename = "trialTimeoutSeconds", default)]
    pub trial_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub approval: Option<String>,
}

fn default_timeout() -> u64 {
    600
}

#[derive(Debug, Deserialize)]
pub struct Workspace {
    #[serde(rename = "worktreeRoot", default = "default_worktree_root")]
    pub worktree_root: String,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            worktree_root: default_worktree_root(),
        }
    }
}

fn default_worktree_root() -> String {
    ".worktrees".into()
}

#[derive(Debug, Deserialize)]
pub struct Oracle {
    #[serde(default = "default_oracle_dialect")]
    pub dialect: String,
    #[serde(rename = "landedSeedBaseline", default)]
    pub landed_seed_baseline: Option<LandedSeedBaselineDescriptor>,
}

impl Default for Oracle {
    fn default() -> Self {
        Oracle {
            dialect: default_oracle_dialect(),
            landed_seed_baseline: None,
        }
    }
}

/// Project-owned, signed pointer to the immutable landed-contract genesis.
/// The manifest itself stays project data; generic runtime code validates its
/// digest and closed schema instead of embedding hundreds of project paths.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedBaselineDescriptor {
    pub schema_version: u32,
    pub path: String,
    pub sha256: String,
}

/// Closed JSON schema for the project-signed, immutable landed-seed genesis.
/// Future relocation/supersession facts are ledger deltas and never rewrite
/// this manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedBaselineManifest {
    pub schema_version: u32,
    pub baseline_tree_sha: String,
    pub scope: LandedSeedBaselineScope,
    pub counts: LandedSeedBaselineCounts,
    pub excluded_unrecorded_missing_targets: Vec<String>,
    pub targets: Vec<LandedSeedBaselineTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedBaselineScope {
    pub declared_pair_audit_through: String,
    pub effective_baseline_through: String,
    pub selection: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedBaselineCounts {
    pub declared_pairs_through_r70: usize,
    pub drifted_declared_pairs_through_r70: usize,
    pub missing_declared_pairs_through_r70: usize,
    pub unique_effective_targets_through_b269: usize,
    pub present_effective_targets: usize,
    pub effective_tombstones: usize,
    pub excluded_unrecorded_missing_targets: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LandedSeedBaselineTargetState {
    Present,
    Tombstone,
}

/// `effectiveSha256` is a required JSON field even for a tombstone, where its
/// value must be explicit `null`.  Wrapping `Option` prevents serde's usual
/// missing-Option compatibility rule from accepting an omitted field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct RequiredNullableSha256(pub Option<String>);

impl RequiredNullableSha256 {
    pub fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }

    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedBaselineTarget {
    pub target: String,
    pub state: LandedSeedBaselineTargetState,
    pub effective_sha256: RequiredNullableSha256,
    pub effective_anchor: LandedSeedBaselineEffectiveAnchor,
    pub grandfathered_drift: bool,
    pub sources: Vec<LandedSeedBaselineSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LandedSeedBaselineEffectiveAnchor {
    SeedRelocated {
        event_id: String,
        sha256: String,
    },
    MigrationBaseline {
        baseline_tree_sha: String,
        sha256: String,
    },
    MigrationTombstone {
        baseline_tree_sha: String,
    },
}

impl LandedSeedBaselineEffectiveAnchor {
    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::SeedRelocated { sha256, .. } | Self::MigrationBaseline { sha256, .. } => {
                Some(sha256)
            }
            Self::MigrationTombstone { .. } => None,
        }
    }

    pub fn event_id(&self) -> Option<&str> {
        match self {
            Self::SeedRelocated { event_id, .. } => Some(event_id),
            Self::MigrationBaseline { .. } | Self::MigrationTombstone { .. } => None,
        }
    }

    pub fn baseline_tree_sha(&self) -> Option<&str> {
        match self {
            Self::MigrationBaseline {
                baseline_tree_sha, ..
            }
            | Self::MigrationTombstone { baseline_tree_sha } => Some(baseline_tree_sha),
            Self::SeedRelocated { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedBaselineSource {
    pub round: String,
    pub task_id: String,
    pub card_path: String,
    pub seed_src: String,
    pub declared_sha256: String,
    pub source_sha256: String,
    pub source_matches_declared: bool,
    pub drifted_from_source: bool,
    pub seed_relocated: Vec<LandedSeedRelocationProvenance>,
    pub task_recorded_event_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LandedSeedRelocationProvenance {
    pub event_id: String,
    pub sha256: String,
}

fn canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_commit_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_event_id(value: &str) -> bool {
    value.len() == 26
        && value.as_bytes()[0].is_ascii_digit()
        && value.as_bytes()[0] <= b'7'
        && value.bytes().all(|byte| {
            byte.is_ascii_digit()
                || (byte.is_ascii_uppercase() && !matches!(byte, b'I' | b'L' | b'O' | b'U'))
        })
}

fn canonical_repo_relative(path: &str) -> bool {
    let parsed = Path::new(path);
    !path.is_empty()
        && path.trim() == path
        && !parsed.is_absolute()
        && !path.contains('\\')
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn task_id_is_canonical(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn round_number(round: &str) -> Option<u64> {
    let number = round.strip_prefix('r')?;
    if number.is_empty()
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    number.parse().ok()
}

fn strictly_sorted_unique<'a>(values: impl IntoIterator<Item = &'a str>) -> bool {
    let mut previous = None;
    for value in values {
        if previous.is_some_and(|previous| previous >= value) {
            return false;
        }
        previous = Some(value);
    }
    true
}

/// Validate the signed pointer without touching the filesystem.  Unknown or
/// missing descriptor fields are rejected by serde before this semantic pass.
pub fn validate_landed_seed_baseline_descriptor(
    descriptor: &LandedSeedBaselineDescriptor,
) -> std::result::Result<(), String> {
    if descriptor.schema_version != 1 {
        return Err(format!(
            "landedSeedBaseline schemaVersion 必须为 1，实际 {}",
            descriptor.schema_version
        ));
    }
    if !canonical_repo_relative(&descriptor.path) || !descriptor.path.ends_with(".json") {
        return Err(format!(
            "landedSeedBaseline path 必须是 canonical repo-relative JSON 路径: {:?}",
            descriptor.path
        ));
    }
    if !canonical_sha256(&descriptor.sha256) {
        return Err("landedSeedBaseline sha256 非 canonical SHA-256".to_string());
    }
    Ok(())
}

/// Parse binding bytes already captured from an exact/signed source.  Machine
/// overlays are deliberately absent: they are not part of authorization.
pub fn parse_binding_bytes(bytes: &[u8]) -> std::result::Result<Binding, String> {
    let binding: Binding = serde_yaml::from_slice(bytes)
        .map_err(|error| format!("PROJECT-BINDING bytes 解析失败: {error}"))?;
    if let Some(descriptor) = binding.oracle.landed_seed_baseline.as_ref() {
        validate_landed_seed_baseline_descriptor(descriptor)?;
    }
    Ok(binding)
}

/// Strictly parse and semantically validate manifest bytes against the signed
/// descriptor.  The caller supplies bytes from the exact tree named by the
/// already-validated source binding; this function performs no ref/path read.
pub fn parse_landed_seed_baseline(
    descriptor: &LandedSeedBaselineDescriptor,
    manifest_bytes: &[u8],
) -> std::result::Result<LandedSeedBaselineManifest, String> {
    validate_landed_seed_baseline_descriptor(descriptor)?;
    let actual = hex::encode(Sha256::digest(manifest_bytes));
    if actual != descriptor.sha256 {
        return Err(format!(
            "landed seed baseline manifest SHA-256 不匹配: expected={} actual={actual}",
            descriptor.sha256
        ));
    }
    let manifest: LandedSeedBaselineManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|error| format!("landed seed baseline manifest 非 closed JSON schema: {error}"))?;
    if manifest.schema_version != descriptor.schema_version {
        return Err(format!(
            "landed seed baseline manifest schemaVersion={} 与 descriptor={} 不一致",
            manifest.schema_version, descriptor.schema_version
        ));
    }
    validate_landed_seed_baseline_manifest(&manifest)?;
    Ok(manifest)
}

/// Pure semantic validation for a parsed genesis.  Counts are derived back
/// from provenance rather than trusted as labels, and all ordered collections
/// are canonical so a digest binds one unambiguous projection.
pub fn validate_landed_seed_baseline_manifest(
    manifest: &LandedSeedBaselineManifest,
) -> std::result::Result<(), String> {
    if manifest.schema_version != 1 || !canonical_commit_sha(&manifest.baseline_tree_sha) {
        return Err("landed seed baseline schemaVersion/baselineTreeSha 非 canonical".to_string());
    }
    let declared_through = round_number(&manifest.scope.declared_pair_audit_through)
        .ok_or_else(|| "declaredPairAuditThrough 必须是 canonical round id".to_string())?;
    let (effective_round, effective_task) = manifest
        .scope
        .effective_baseline_through
        .split_once('/')
        .ok_or_else(|| "effectiveBaselineThrough 必须是 rN/taskId".to_string())?;
    if round_number(effective_round).is_none()
        || !task_id_is_canonical(effective_task)
        || manifest.scope.selection.trim().is_empty()
        || manifest.scope.selection.trim() != manifest.scope.selection
    {
        return Err("landed seed baseline scope 非 canonical".to_string());
    }
    if !strictly_sorted_unique(
        manifest
            .excluded_unrecorded_missing_targets
            .iter()
            .map(String::as_str),
    ) || manifest
        .excluded_unrecorded_missing_targets
        .iter()
        .any(|path| !canonical_repo_relative(path))
    {
        return Err(
            "excludedUnrecordedMissingTargets 必须 canonical、严格排序且无重复".to_string(),
        );
    }
    if !strictly_sorted_unique(manifest.targets.iter().map(|target| target.target.as_str())) {
        return Err("landed seed baseline targets 必须按 path 严格排序且无重复".to_string());
    }

    let excluded = manifest
        .excluded_unrecorded_missing_targets
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut declared_pairs = excluded.len();
    let mut drifted_pairs = 0usize;
    let mut present = 0usize;
    let mut tombstones = 0usize;

    for target in &manifest.targets {
        if !canonical_repo_relative(&target.target) || excluded.contains(target.target.as_str()) {
            return Err(format!(
                "landed seed target 非 canonical 或同时被 excluded: {}",
                target.target
            ));
        }
        if target.sources.is_empty() {
            return Err(format!(
                "landed seed target 缺 provenance sources: {}",
                target.target
            ));
        }
        let mut previous_source: Option<(u64, &str, &str)> = None;
        let effective_sha = target.effective_sha256.0.as_deref();
        let anchor_sha = target.effective_anchor.sha256();
        match target.state {
            LandedSeedBaselineTargetState::Present => {
                present += 1;
                if effective_sha.is_none()
                    || effective_sha.is_some_and(|sha| !canonical_sha256(sha))
                    || anchor_sha != effective_sha
                    || matches!(
                        &target.effective_anchor,
                        LandedSeedBaselineEffectiveAnchor::MigrationTombstone { .. }
                    )
                {
                    return Err(format!(
                        "present target effective SHA/anchor 非 canonical: {}",
                        target.target
                    ));
                }
            }
            LandedSeedBaselineTargetState::Tombstone => {
                tombstones += 1;
                if effective_sha.is_some()
                    || !matches!(
                        &target.effective_anchor,
                        LandedSeedBaselineEffectiveAnchor::MigrationTombstone { .. }
                    )
                    || target.grandfathered_drift
                {
                    return Err(format!(
                        "tombstone target shape 非 canonical: {}",
                        target.target
                    ));
                }
            }
        }
        match &target.effective_anchor {
            LandedSeedBaselineEffectiveAnchor::SeedRelocated { event_id, sha256 } => {
                if !canonical_event_id(event_id) || !canonical_sha256(sha256) {
                    return Err(format!(
                        "seed-relocated effective anchor 非 canonical: {}",
                        target.target
                    ));
                }
                let anchored = target
                    .sources
                    .iter()
                    .flat_map(|source| &source.seed_relocated)
                    .any(|anchor| {
                        anchor.event_id.as_str() == event_id.as_str()
                            && anchor.sha256.as_str() == sha256.as_str()
                    });
                if !anchored {
                    return Err(format!(
                        "effective SeedRelocated 不在 provenance 中: {}",
                        target.target
                    ));
                }
            }
            LandedSeedBaselineEffectiveAnchor::MigrationBaseline {
                baseline_tree_sha,
                sha256,
            } => {
                if baseline_tree_sha != &manifest.baseline_tree_sha || !canonical_sha256(sha256) {
                    return Err(format!(
                        "migration-baseline anchor 非 canonical: {}",
                        target.target
                    ));
                }
            }
            LandedSeedBaselineEffectiveAnchor::MigrationTombstone { baseline_tree_sha } => {
                if baseline_tree_sha != &manifest.baseline_tree_sha {
                    return Err(format!(
                        "migration-tombstone tree anchor 漂移: {}",
                        target.target
                    ));
                }
            }
        }

        let mut any_drift = false;
        for source in &target.sources {
            let source_round = round_number(&source.round)
                .ok_or_else(|| format!("source round 非 canonical: {}", source.round))?;
            if !task_id_is_canonical(&source.task_id)
                || !canonical_repo_relative(&source.card_path)
                || !canonical_repo_relative(&source.seed_src)
                || source.card_path
                    != format!(
                        "coordination/rounds/{}/tasks/{}.md",
                        source.round, source.task_id
                    )
                || !source.seed_src.starts_with(&format!(
                    "coordination/rounds/{}/seeds/{}/",
                    source.round, source.task_id
                ))
            {
                return Err(format!(
                    "source path/task provenance 非 canonical: {}/{}",
                    source.round, source.task_id
                ));
            }
            let source_key = (
                source_round,
                source.task_id.as_str(),
                source.seed_src.as_str(),
            );
            if previous_source.is_some_and(|previous| previous >= source_key) {
                return Err(format!("sources 未严格排序或重复: {}", target.target));
            }
            previous_source = Some(source_key);
            if !canonical_sha256(&source.declared_sha256)
                || !canonical_sha256(&source.source_sha256)
                || source.source_matches_declared
                    != (source.declared_sha256 == source.source_sha256)
            {
                return Err(format!(
                    "source hash provenance 非 canonical: {}",
                    target.target
                ));
            }
            if source_round <= declared_through {
                declared_pairs += 1;
                if source.drifted_from_source {
                    drifted_pairs += 1;
                }
            }
            if matches!(target.state, LandedSeedBaselineTargetState::Present)
                && source.drifted_from_source
                    != (Some(source.source_sha256.as_str()) != effective_sha)
            {
                return Err(format!(
                    "driftedFromSource 与 effective SHA 不一致: {}",
                    target.target
                ));
            }
            if matches!(target.state, LandedSeedBaselineTargetState::Tombstone)
                && source.drifted_from_source
            {
                return Err(format!(
                    "tombstone source 不得伪称 grandfathered drift: {}",
                    target.target
                ));
            }
            any_drift |= source.drifted_from_source;

            if !strictly_sorted_unique(
                source
                    .seed_relocated
                    .iter()
                    .map(|anchor| anchor.event_id.as_str()),
            ) || source.seed_relocated.iter().any(|anchor| {
                !canonical_event_id(&anchor.event_id) || !canonical_sha256(&anchor.sha256)
            }) {
                return Err(format!(
                    "SeedRelocated provenance 非 canonical: {}",
                    target.target
                ));
            }
            if !strictly_sorted_unique(source.task_recorded_event_ids.iter().map(String::as_str))
                || source
                    .task_recorded_event_ids
                    .iter()
                    .any(|event_id| !canonical_event_id(event_id))
            {
                return Err(format!(
                    "TaskRecorded provenance 非 canonical: {}",
                    target.target
                ));
            }
        }
        if target.grandfathered_drift != any_drift
            || (target.grandfathered_drift
                && !matches!(
                    &target.effective_anchor,
                    LandedSeedBaselineEffectiveAnchor::MigrationBaseline { .. }
                ))
        {
            return Err(format!(
                "grandfathered drift/anchor 不一致: {}",
                target.target
            ));
        }
    }

    let counts = &manifest.counts;
    let actual_missing = tombstones + excluded.len();
    if counts.declared_pairs_through_r70 != declared_pairs
        || counts.drifted_declared_pairs_through_r70 != drifted_pairs
        || counts.missing_declared_pairs_through_r70 != actual_missing
        || counts.unique_effective_targets_through_b269 != manifest.targets.len()
        || counts.present_effective_targets != present
        || counts.effective_tombstones != tombstones
        || counts.excluded_unrecorded_missing_targets != excluded.len()
    {
        return Err(format!(
            "landed seed baseline counts 不一致: declared={declared_pairs} drifted={drifted_pairs} missing={actual_missing} unique={} present={present} tombstones={tombstones} excluded={}",
            manifest.targets.len(),
            excluded.len()
        ));
    }
    Ok(())
}

fn default_oracle_dialect() -> String {
    "vitest".into()
}

#[derive(Debug, Default, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub ecosystems: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Scope {
    #[serde(rename = "protectedPaths", default)]
    pub protected_paths: Vec<String>,
    #[serde(rename = "reviewRequiredPaths", default)]
    pub review_required_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Git {
    #[serde(rename = "implementerMayCommit", default = "default_true")]
    pub implementer_may_commit: bool,
    #[serde(rename = "implementerMayMerge", default)]
    pub implementer_may_merge: bool,
    #[serde(rename = "pushPolicy", default = "default_push_policy")]
    pub push_policy: String,
    #[serde(rename = "mergePolicy", default = "default_merge_policy")]
    pub merge_policy: String,
}

impl Default for Git {
    fn default() -> Self {
        Git {
            implementer_may_commit: true,
            implementer_may_merge: false,
            push_policy: default_push_policy(),
            merge_policy: default_merge_policy(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_push_policy() -> String {
    "forbidden".into()
}

fn default_merge_policy() -> String {
    "ff-only-else-no-ff".into()
}

impl Binding {
    pub fn has_ecosystem(&self, ecosystem: &str) -> bool {
        self.project
            .ecosystems
            .iter()
            .any(|value| value.eq_ignore_ascii_case(ecosystem))
    }
}

/// 按任务卡顺序把门名解析到 binding 的可用命令名集合。
///
/// 重复项保留首现；任一名称缺失时一次性返回全部缺名，禁止静默缩窄门面。
pub fn resolve_gates(
    names: &[String],
    available: &[String],
) -> std::result::Result<Vec<String>, String> {
    let mut resolved = Vec::new();
    let mut missing = Vec::new();
    for name in names {
        if resolved.contains(name) || missing.contains(name) {
            continue;
        }
        if available.contains(name) {
            resolved.push(name.clone());
        } else {
            missing.push(name.clone());
        }
    }
    if missing.is_empty() {
        Ok(resolved)
    } else {
        Err(format!("绑定缺命令: {}", missing.join(", ")))
    }
}

/// Closed V1 gate lane selected by a runtime boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateLaneV1 {
    /// Narrow collection lane whose derived inputs must be closed.
    Candidate,
    /// Full lane used around merge-strength boundaries.
    Merge,
    /// Signed compatibility lane and escalation destination.
    Fast,
}

/// Complete, already-derived inputs to the pure V1 lane resolver.
///
/// Callers derive the two closure booleans from signed card seed targets and
/// the source-reader descriptor. The resolver never guesses a missing edge or
/// command and therefore cannot silently shrink a candidate run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateLaneInputsV1 {
    /// Boundary lane requested by the caller.
    pub lane: GateLaneV1,
    /// Ordered candidate command references from the committed binding.
    pub candidate: Vec<String>,
    /// Ordered merge command references from the committed binding.
    pub merge: Vec<String>,
    /// Ordered full-strength upgrade command references.
    pub fast: Vec<String>,
    /// Signed task-card `gates.fast` floor that merge and fast must cover.
    pub card_fast: Vec<String>,
    /// Command references the caller can resolve to concrete argv.
    pub known_commands: Vec<String>,
    /// Whether every candidate seed target was derived from this card alone.
    pub seed_targets_closed: bool,
    /// Whether every production `sourceReaderClosure` edge has a runnable target.
    pub source_readers_closed: bool,
}

/// Fail-closed result of resolving one V1 gate lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateLaneDecisionV1 {
    /// Execute these command references in their signed order.
    Commands(Vec<String>),
    /// Execute the signed fast lane and durably record this reason.
    UpgradeToFast {
        /// Stable human-readable explanation suitable for GateLaneEscalated.
        reason: String,
    },
}

fn safe_gate_ref(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn checked_gate_refs<'a>(
    label: &str,
    values: &'a [String],
    allow_empty: bool,
) -> std::result::Result<BTreeSet<&'a str>, String> {
    if values.is_empty() && !allow_empty {
        return Err(format!("{label} gate lane 为空"));
    }
    let mut seen = BTreeSet::new();
    for value in values {
        if !safe_gate_ref(value) {
            return Err(format!("{label} gate ref 非安全 component: {value:?}"));
        }
        if !seen.insert(value.as_str()) {
            return Err(format!("{label} gate ref 重复: {value}"));
        }
    }
    Ok(seen)
}

fn validate_full_lane(
    label: &str,
    commands: &[String],
    card_fast: &BTreeSet<&str>,
    known_commands: &BTreeSet<&str>,
) -> std::result::Result<(), String> {
    let lane = checked_gate_refs(label, commands, false)?;
    let missing_floor = card_fast.difference(&lane).copied().collect::<Vec<_>>();
    if !missing_floor.is_empty() {
        return Err(format!(
            "binding gates.{label} 弱于 signed card gates.fast，缺: {}",
            missing_floor.join(", ")
        ));
    }
    let unknown = lane.difference(known_commands).copied().collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!(
            "binding gates.{label} 含未知 command: {}",
            unknown.join(", ")
        ));
    }
    Ok(())
}

/// Resolve a V1 gate lane without filesystem or ledger side effects.
///
/// Both full-strength lanes are first checked against the signed card floor.
/// A candidate closure failure or candidate-only unknown command returns an
/// explicit fast upgrade; an invalid fast/merge lane is an error because no
/// stronger modeled destination exists.
pub fn resolve_gate_lane_v1(
    inputs: &GateLaneInputsV1,
) -> std::result::Result<GateLaneDecisionV1, String> {
    let card_fast = checked_gate_refs("card fast", &inputs.card_fast, false)?;
    let known_commands = checked_gate_refs("known commands", &inputs.known_commands, false)?;
    validate_full_lane("merge", &inputs.merge, &card_fast, &known_commands)?;
    validate_full_lane("fast", &inputs.fast, &card_fast, &known_commands)?;

    match inputs.lane {
        GateLaneV1::Merge => Ok(GateLaneDecisionV1::Commands(inputs.merge.clone())),
        GateLaneV1::Fast => Ok(GateLaneDecisionV1::Commands(inputs.fast.clone())),
        GateLaneV1::Candidate => {
            let candidate = checked_gate_refs("candidate", &inputs.candidate, false)?;
            let mut reasons = Vec::new();
            if !inputs.seed_targets_closed {
                reasons.push("seed targets are not closed over the signed card".to_string());
            }
            if !inputs.source_readers_closed {
                reasons.push("source-reader closure is unknown or drifted".to_string());
            }
            let unknown = candidate
                .difference(&known_commands)
                .copied()
                .collect::<Vec<_>>();
            if !unknown.is_empty() {
                reasons.push(format!(
                    "candidate lane contains unknown command: {}",
                    unknown.join(", ")
                ));
            }
            if reasons.is_empty() {
                Ok(GateLaneDecisionV1::Commands(inputs.candidate.clone()))
            } else {
                Ok(GateLaneDecisionV1::UpgradeToFast {
                    reason: reasons.join("; "),
                })
            }
        }
    }
}

fn rust_gate_command_names(b: &Binding) -> (&'static str, &'static str) {
    if b.has_ecosystem("node") {
        ("rustTest", "rustCheck")
    } else {
        ("testFast", "check")
    }
}

/// Rust 生态绑定下测试与检查门命令的 `--locked` 规则机器化（B106）。
///
/// Rust-only bindings use `testFast`/`check`; Rust+Node bindings use the
/// unambiguous `rustTest`/`rustCheck` pair so Node commands cannot satisfy a
/// Cargo floor. Both selected commands are checked and all failures aggregate:
///   1. 命令缺失（binding 未声明该命令名）；
///   2. 命令存在但 argv 为空；
///   3. `--locked` 未作为独立 argv 出现在 `--` 终止符**之前**（把 `--` 之后的
///      `--locked` 当 Cargo 选项而不当门强制，拒绝）。
/// 缺命令、空 argv、缺 flag 一次性聚合返回全部错误文案。
/// 非绑定为 rust 生态的项目不强制（直接 Ok）。
///
/// 注意：本函数只看 binding 本身，不看根目录是否有 Cargo 标记。`load()` 在
/// 真实文件系统 Rust 项目根（`root/Cargo.toml` 或 `root/orch/Cargo.toml`）时才
/// 调用本严格校验；合成 fixture 根（无 Cargo 标记）与非 Rust 绑定在 `load()`
/// 处跳过，以保持既有合成测试（如 `merge_irreversible` 仅 `postGate`）不受扰。
pub fn validate_locked_rust_gates(b: &Binding) -> std::result::Result<(), Vec<String>> {
    if !b.has_ecosystem("rust") {
        return Ok(());
    }
    let mut errors: Vec<String> = Vec::new();
    let (test_name, check_name) = rust_gate_command_names(b);
    for name in [test_name, check_name] {
        let Some(spec) = b.commands.get(name) else {
            errors.push(format!(
                "命令 {name} 缺失：Rust 绑定必须声明 {name} 且含独立 --locked"
            ));
            continue;
        };
        if spec.argv.is_empty() {
            errors.push(format!("命令 {name} argv 为空：必须含独立 --locked"));
        } else if !has_locked_before_terminator(&spec.argv) {
            errors.push(format!(
                "命令 {name} 缺独立 --locked：argv 必须在 `--` 终止符之前含独立 --locked"
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Require a Rust binding's Cargo-check command to cover every Cargo target.
///
/// Rust-only bindings select `check`, while Rust+Node bindings select
/// `rustCheck`. The selected command must exist, have a non-empty argv, and
/// contain an independent `--all-targets` token before the first `--`
/// terminator. A joined string or a token after the terminator is not
/// authorization. Non-Rust bindings keep their historical semantics.
pub fn validate_rust_check_all_targets(b: &Binding) -> std::result::Result<(), Vec<String>> {
    if !b.has_ecosystem("rust") {
        return Ok(());
    }

    let (_, check_name) = rust_gate_command_names(b);
    let error = match b.commands.get(check_name) {
        None => Some(format!(
            "命令 {check_name} 缺失：Rust 绑定必须声明 {check_name} 且含独立 --all-targets"
        )),
        Some(spec) if spec.argv.is_empty() => {
            Some(format!(
                "命令 {check_name} argv 为空：必须含独立 --all-targets"
            ))
        }
        Some(spec) if !has_token_before_terminator(&spec.argv, "--all-targets") => Some(
            format!(
                "命令 {check_name} 缺独立 --all-targets：argv 必须在 `--` 终止符之前含独立 --all-targets"
            ),
        ),
        Some(_) => None,
    };

    match error {
        Some(error) => Err(vec![error]),
        None => Ok(()),
    }
}

/// Aggregate every Rust gate-floor violation so callers never repair
/// `--locked` only to discover a hidden `--all-targets` failure afterwards.
pub(crate) fn validate_rust_gate_floors(b: &Binding) -> std::result::Result<(), Vec<String>> {
    let mut errors = validate_locked_rust_gates(b).err().unwrap_or_default();
    errors.extend(validate_rust_check_all_targets(b).err().unwrap_or_default());
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Apply the shared Rust gate floors only at a real Cargo project root.
///
/// Both [`load`] and production `orch plan` call this wrapper so filesystem
/// fixtures without `Cargo.toml` retain their historical synthetic semantics,
/// while a real Rust project cannot take different admission paths.
pub(crate) fn validate_rust_gate_floors_at_root(
    root: &Path,
    b: &Binding,
) -> std::result::Result<(), Vec<String>> {
    if is_rust_project_root(root) {
        validate_rust_gate_floors(b)
    } else {
        Ok(())
    }
}

/// 判断 argv 是否在 `--` 终止符之前含独立 `--locked` token。
fn has_locked_before_terminator(argv: &[String]) -> bool {
    has_token_before_terminator(argv, "--locked")
}

fn has_token_before_terminator(argv: &[String], required: &str) -> bool {
    argv.iter()
        .take_while(|tok| *tok != "--")
        .any(|tok| tok == required)
}

/// 判断根目录是否为真实文件系统 Rust 项目根（planner r46-repair 决策）。
///
/// 命中 `root/Cargo.toml`（顶层 Cargo 工作区/包）或 `root/orch/Cargo.toml`
/// （自举布局：外层仓无顶层 Cargo.toml，orch 子目录才是工作区根）之一即视为
/// 真实 Rust 根。合成 fixture 根（无任一标记）返回 false，`load()` 据此跳过
/// 严格 Rust 门校验，保持既有合成测试不受扰。
/// 无外部消费者（仅本文件 `load()` 使用），保持私有（reviewer r46 HOLD 修复）。
fn is_rust_project_root(root: &Path) -> bool {
    root.join("Cargo.toml").is_file() || root.join("orch/Cargo.toml").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    // B108 scratch 纪律（reviewer r46 HOLD 修复）：测试 scratch 必须落在调用
    // worktree 的 `orch/target/test-tmp`，目录名含 pid 与模块级 AtomicU64
    // fetch_add 序号（线程安全、无时钟依赖），不得使用 std::env::temp_dir()。
    static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// 从 CARGO_MANIFEST_DIR（<worktree>/orch/crates/orch-host）上溯两级到
    /// worktree/orch，在 `target/test-tmp/{tag}-{pid}-{seq}` 建目录并返回。
    fn b106_scratch_dir(tag: &str) -> std::path::PathBuf {
        let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let dir = orch_root.join("target/test-tmp").join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn repository_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("CARGO_MANIFEST_DIR 上溯三级应为外层仓根")
            .to_path_buf()
    }

    fn genesis_value() -> serde_json::Value {
        let bytes =
            std::fs::read(repository_root().join("coordination/frozen-contract-baseline-v1.json"))
                .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn descriptor_for(bytes: &[u8]) -> LandedSeedBaselineDescriptor {
        LandedSeedBaselineDescriptor {
            schema_version: 1,
            path: "coordination/frozen-contract-baseline-v1.json".to_string(),
            sha256: hex::encode(Sha256::digest(bytes)),
        }
    }

    fn parse_mutated_genesis(value: &serde_json::Value) -> std::result::Result<(), String> {
        let bytes = serde_json::to_vec(value).unwrap();
        parse_landed_seed_baseline(&descriptor_for(&bytes), &bytes).map(|_| ())
    }

    #[test]
    fn signed_genesis_descriptor_and_manifest_are_strictly_valid() {
        let root = repository_root();
        let binding_bytes = std::fs::read(root.join("coordination/PROJECT-BINDING.yaml")).unwrap();
        let binding = parse_binding_bytes(&binding_bytes).unwrap();
        assert!(
            binding.oracle.landed_seed_baseline.is_none(),
            "schema 3 live binding must not retain the legacy frozen baseline descriptor"
        );
        let bytes = std::fs::read(
            root.join("coordination/frozen-contract-baseline-v1.json"),
        )
        .unwrap();
        let descriptor = descriptor_for(&bytes);
        let manifest = parse_landed_seed_baseline(&descriptor, &bytes).unwrap();
        assert_eq!(manifest.counts.declared_pairs_through_r70, 243);
        assert_eq!(manifest.counts.drifted_declared_pairs_through_r70, 36);
        assert_eq!(manifest.counts.missing_declared_pairs_through_r70, 4);
        assert_eq!(manifest.counts.unique_effective_targets_through_b269, 225);
        assert_eq!(manifest.counts.present_effective_targets, 224);
        assert_eq!(manifest.counts.effective_tombstones, 1);
        assert_eq!(manifest.counts.excluded_unrecorded_missing_targets, 3);
    }

    #[test]
    fn baseline_descriptor_rejects_unknown_missing_path_and_hash() {
        let hash = "a".repeat(64);
        let unknown = format!(
            "oracle:\n  landedSeedBaseline:\n    schemaVersion: 1\n    path: coordination/baseline.json\n    sha256: {hash}\n    unknown: true\n"
        );
        assert!(parse_binding_bytes(unknown.as_bytes()).is_err());
        let missing = b"oracle:\n  landedSeedBaseline:\n    schemaVersion: 1\n    path: coordination/baseline.json\n";
        assert!(parse_binding_bytes(missing).is_err());

        let mut descriptor = descriptor_for(b"{}");
        descriptor.path = "../baseline.json".to_string();
        assert!(validate_landed_seed_baseline_descriptor(&descriptor)
            .unwrap_err()
            .contains("path"));
        descriptor.path = "coordination/baseline.json".to_string();
        descriptor.sha256 = "A".repeat(64);
        assert!(validate_landed_seed_baseline_descriptor(&descriptor)
            .unwrap_err()
            .contains("sha256"));
    }

    #[test]
    fn baseline_manifest_rejects_unknown_missing_duplicate_unsorted_and_bad_counts() {
        let mut value = genesis_value();
        value["unknown"] = serde_json::json!(true);
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("unknown"));

        let mut value = genesis_value();
        value["targets"][0]
            .as_object_mut()
            .unwrap()
            .remove("effectiveSha256");
        let error = parse_mutated_genesis(&value).unwrap_err();
        assert!(
            error.contains("effective") || error.contains("missing field"),
            "unexpected error: {error}"
        );

        let mut value = genesis_value();
        let duplicate = value["targets"][0]["target"].clone();
        value["targets"][1]["target"] = duplicate;
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("严格排序"));

        let mut value = genesis_value();
        value["targets"].as_array_mut().unwrap().swap(0, 1);
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("严格排序"));

        let mut value = genesis_value();
        value["counts"]["presentEffectiveTargets"] = serde_json::json!(223);
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("counts"));
    }

    #[test]
    fn baseline_manifest_rejects_invalid_paths_hashes_and_anchors() {
        let mut value = genesis_value();
        value["targets"][0]["target"] = serde_json::json!("../escape.rs");
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("target"));

        let mut value = genesis_value();
        value["targets"][0]["sources"][0]["sourceSha256"] = serde_json::json!("A".repeat(64));
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("hash provenance"));

        let mut value = genesis_value();
        value["targets"][0]["effectiveAnchor"]["eventId"] = serde_json::json!("forged");
        assert!(parse_mutated_genesis(&value)
            .unwrap_err()
            .contains("effective anchor"));

        let bytes = serde_json::to_vec(&genesis_value()).unwrap();
        let mut descriptor = descriptor_for(&bytes);
        descriptor.sha256 = "f".repeat(64);
        assert!(parse_landed_seed_baseline(&descriptor, &bytes)
            .unwrap_err()
            .contains("不匹配"));
    }

    #[test]
    fn missing_scope_and_git_use_legacy_safe_defaults() {
        let binding: Binding = serde_yaml::from_str(
            "commands: {}\nunknownTopLevelField: tolerated\noracle:\n  futureSibling: tolerated\n",
        )
        .unwrap();
        assert!(binding.scope.protected_paths.is_empty());
        assert!(binding.scope.review_required_paths.is_empty());
        assert!(binding.git.implementer_may_commit);
        assert!(!binding.git.implementer_may_merge);
        assert_eq!(binding.git.push_policy, "forbidden");
        assert_eq!(binding.git.merge_policy, "ff-only-else-no-ff");
    }

    #[test]
    fn trial_timeout_is_optional_and_explicit_value_is_preserved() {
        let binding: Binding = serde_yaml::from_str(
            "commands:\n  implicit:\n    argv: [cargo, test]\n    timeoutSeconds: 7\n  explicit:\n    argv: [cargo, test]\n    timeoutSeconds: 7\n    trialTimeoutSeconds: 11\n",
        )
        .unwrap();
        assert_eq!(binding.commands["implicit"].trial_timeout_seconds, None);
        assert_eq!(binding.commands["explicit"].trial_timeout_seconds, Some(11));
        assert_eq!(binding.commands["implicit"].timeout_seconds, 7);
    }

    // B106 严格聚合：缺命令本身也报错（planner r46-repair 决策）。
    fn rust_binding_with(yaml: &str) -> Binding {
        serde_yaml::from_str(&format!("project: {{ecosystems: [rust]}}\n{yaml}\n")).unwrap()
    }

    #[test]
    fn strict_missing_testfast_and_check_are_both_aggregated() {
        // 既缺 testFast 又缺 check：两条错误一次性聚合，文案各自含命令名。
        let b = rust_binding_with("commands:\n  postGate:\n    argv: [\"true\"]\n");
        let errors = validate_locked_rust_gates(&b).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("testFast")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("check")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("缺失")), "{errors:?}");
    }

    #[test]
    fn strict_missing_only_testfast_aggregates_testfast_name() {
        // 只缺 testFast（check 存在且 locked）：错误文案含 testFast 不含 check 缺失。
        let b = rust_binding_with(
            "commands:\n  check:\n    argv: [cargo, check, --workspace, --locked]\n",
        );
        let errors = validate_locked_rust_gates(&b).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("testFast")), "{errors:?}");
        // check 存在且 locked，不应有 check 缺失/缺 locked 错误。
        assert!(
            !errors
                .iter()
                .any(|e| e.contains("命令 check") && e.contains("缺失")),
            "{errors:?}"
        );
    }

    #[test]
    fn strict_empty_argv_is_aggregated_with_command_name() {
        // 命令存在但 argv 为空：聚合报错，文案含命令名。
        let b = rust_binding_with(
            "commands:\n  testFast:\n    argv: []\n  check:\n    argv: [cargo, check, --locked]\n",
        );
        let errors = validate_locked_rust_gates(&b).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("testFast") && e.contains("argv 为空")),
            "{errors:?}"
        );
    }

    #[test]
    fn strict_non_rust_binding_skips_gate() {
        // 非 rust 生态：即便缺 testFast/check 也不强制（直接 Ok）。
        let b: Binding = serde_yaml::from_str(
            "project: {ecosystems: [node]}\ncommands:\n  postGate:\n    argv: [\"true\"]\n",
        )
        .unwrap();
        assert!(validate_locked_rust_gates(&b).is_ok());
    }

    #[test]
    fn rust_check_all_targets_is_an_independent_pre_terminator_token() {
        let good = rust_binding_with(
            "commands:\n  check:\n    argv: [cargo, check, --all-targets, --locked]\n",
        );
        assert!(validate_rust_check_all_targets(&good).is_ok());

        for argv in [
            "[cargo, check, --locked]",
            "[cargo, \"check --all-targets\", --locked]",
            "[cargo, check, --locked, --, --all-targets]",
            "[]",
        ] {
            let binding = rust_binding_with(&format!("commands:\n  check:\n    argv: {argv}\n"));
            let errors = validate_rust_check_all_targets(&binding).unwrap_err();
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("check") && error.contains("--all-targets")),
                "{argv}: {errors:?}"
            );
        }
    }

    #[test]
    fn rust_gate_floors_report_locked_and_all_targets_together() {
        let binding = rust_binding_with(
            "commands:\n  testFast:\n    argv: [cargo, test, --locked]\n  check:\n    argv: [cargo, check]\n",
        );
        let errors = validate_rust_gate_floors(&binding).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("check") && error.contains("--locked")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("check") && error.contains("--all-targets")),
            "{errors:?}"
        );
    }

    #[test]
    fn non_rust_binding_keeps_historical_check_semantics() {
        let binding: Binding = serde_yaml::from_str(
            "project: {ecosystems: [node]}\ncommands:\n  check: {argv: [npm, test]}\n",
        )
        .unwrap();
        assert!(validate_rust_check_all_targets(&binding).is_ok());
        assert!(validate_rust_gate_floors(&binding).is_ok());
    }

    #[test]
    fn is_rust_project_root_detects_top_level_and_nested_cargo_marker() {
        // 顶层 Cargo.toml
        let tmp = b106_scratch_dir("orch-b106-rust-root-toplevel");
        std::fs::write(tmp.join("Cargo.toml"), "[workspace]\n").unwrap();
        assert!(is_rust_project_root(&tmp));
        // 嵌套 orch/Cargo.toml（自举布局）
        let tmp2 = b106_scratch_dir("orch-b106-rust-root-nested");
        std::fs::create_dir_all(tmp2.join("orch")).unwrap();
        std::fs::write(tmp2.join("orch/Cargo.toml"), "[workspace]\n").unwrap();
        assert!(is_rust_project_root(&tmp2));
        // 合成 fixture 根：无任一标记 → false
        let tmp3 = b106_scratch_dir("orch-b106-synth-root");
        assert!(!is_rust_project_root(&tmp3));
        std::fs::remove_dir_all(&tmp).unwrap();
        std::fs::remove_dir_all(&tmp2).unwrap();
        std::fs::remove_dir_all(&tmp3).unwrap();
    }

    // load 级负向证明：真实 Rust 项目根 + 缺 testFast/check 的 malformed binding
    // 必须在执行前被 load() 拒绝（planner r46-repair 决策）。
    #[test]
    fn load_rejects_malformed_binding_at_real_rust_root() {
        let root = b106_scratch_dir("orch-b106-load-reject");
        std::fs::create_dir_all(root.join("orch")).unwrap();
        // 真实 Rust 根标记（自举布局：orch/Cargo.toml）
        std::fs::write(root.join("orch/Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::create_dir_all(root.join("coordination")).unwrap();
        // malformed：声明 rust 生态但只给 postGate，缺 testFast 与 check
        std::fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            "project: {ecosystems: [rust]}\ncommands:\n  postGate:\n    argv: [\"true\"]\n",
        )
        .unwrap();
        let err = load(&root).unwrap_err().to_string();
        assert!(err.contains("testFast"), "{err}");
        assert!(err.contains("check"), "{err}");
        assert!(err.contains("缺失"), "{err}");
        assert!(err.contains("--all-targets"), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    // load 级对照：合成 fixture 根（无 Cargo 标记）即便缺 testFast/check 也跳过
    // 严格 Rust 门，保持既有合成测试（merge_irreversible）不受扰。
    #[test]
    fn load_skips_strict_gate_at_synthetic_fixture_root() {
        let root = b106_scratch_dir("orch-b106-load-synth");
        std::fs::create_dir_all(root.join("coordination")).unwrap();
        // 无 Cargo.toml / orch/Cargo.toml 标记 → 合成 fixture 根
        std::fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            "project: {ecosystems: [rust]}\ncommands:\n  postGate:\n    argv: [\"true\"]\n",
        )
        .unwrap();
        let b = load(&root).expect("合成 fixture 根跳过严格 Rust 门，不应拒绝");
        assert!(b.has_ecosystem("rust"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

/// Load the project binding, enforce the shared real-Rust gate floors, then
/// apply the optional machine-local argv[0] overlay.
///
/// Validation precedes the overlay so `binding::load` and authorized
/// `orch plan` judge the same signed command tokens, including independent
/// pre-terminator `--locked` and `--all-targets` requirements.
pub fn load(root: &Path) -> Result<Binding> {
    let p = root.join("coordination/PROJECT-BINDING.yaml");
    let text = fs::read_to_string(&p).with_context(|| format!("读取绑定失败: {}", p.display()))?;
    let mut binding = parse_binding_bytes(text.as_bytes())
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("解析绑定失败: {}", p.display()))?;
    // 只在真实文件系统 Rust 项目根强制严格 Rust 门（planner r46-repair 决策）：
    // 合成 fixture 根（无 Cargo.toml/orch/Cargo.toml 标记）与非 Rust 绑定跳过，
    // 保持既有合成测试（如 merge_irreversible 仅 postGate）不受扰。
    validate_rust_gate_floors_at_root(root, &binding)
        .map_err(|errs| anyhow::anyhow!("Rust 门参数 floor 校验失败: {}", errs.join("; ")))?;
    // B151 便携性分层：machine.yaml 覆盖 argv[0]。若本机有 .orch/machine.yaml
    // 且声明的工具键（如 cargo）与某命令 argv[0] 完全相等（可移植默认是裸名），
    // 用 machine 路径替换 argv[0]——这是 detached worktree / 不同机器上 cargo
    // 不在 PATH 时仍能跑门的关键。无 machine.yaml 或无匹配键 → argv 不变。
    apply_machine_overlay(root, &mut binding)?;
    Ok(binding)
}

/// B151 便携性分层：从 `<root>/.orch/machine.yaml` 或主仓 `.orch/machine.yaml`
///（worktree 经 `git rev-parse --git-common-dir` 上溯到主仓）加载本机工具路径，
/// 覆盖 binding 中 argv[0] 与某工具键相等的命令。无 machine.yaml 或无匹配 → 不动。
///
/// **绝不裸名/PATH 猜测**：argv[0] 若是裸 `cargo` 且无 machine.yaml 覆盖，则保留裸名
/// （将由调用方负责 PATH；本机 doctor/AGENTS.md 指引使用者提供 machine.yaml）。
/// 这是 design 意图：可移植默认 = 裸名，本机路径 = machine.yaml 覆盖层。
fn apply_machine_overlay(root: &Path, binding: &mut Binding) -> Result<()> {
    let machine = match load_machine_config(root)? {
        Some(cfg) => cfg,
        None => match resolve_main_repo_machine_config(root)? {
            Some(cfg) => cfg,
            None => return Ok(()),
        },
    };
    for spec in binding.commands.values_mut() {
        if let Some(first) = spec.argv.first_mut() {
            if let Some(machine_path) = machine.tools.get(first) {
                *first = machine_path.clone();
            }
        }
    }
    Ok(())
}

/// 从 `git rev-parse --git-common-dir` 上溯到主仓，读主仓的 `.orch/machine.yaml`。
/// detached worktree（如 postmerge 复跑门现场）本机只有 git-tracked 文件，
/// 没有 `.orch/machine.yaml`（gitignored）；但其主仓可能有。无 git / 无文件 → None。
pub fn resolve_main_repo_machine_config(root: &Path) -> Result<Option<MachineConfig>> {
    let common_dir = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--git-common-dir")
        .output()
        .context("git rev-parse --git-common-dir 失败")?;
    if !common_dir.status.success() {
        return Ok(None);
    }
    let common_dir_str = String::from_utf8_lossy(&common_dir.stdout)
        .trim()
        .to_string();
    if common_dir_str.is_empty() {
        return Ok(None);
    }
    let main_root = Path::new(&common_dir_str)
        .parent()
        .filter(|p| p.is_dir())
        .map(std::path::absolute);
    let Some(main_root) = main_root else {
        return Ok(None);
    };
    let main_root = main_root.context("规范化主仓根为绝对路径失败")?;
    load_machine_config(&main_root)
}

// ─────────────────────────── B151 便携性分层 ───────────────────────────
//
// 本机绝对路径（如 `/Users/admin/.cargo/bin/cargo`）此前硬编码在
// PROJECT-BINDING.yaml 的 gate 命令 argv[0]——换机器即坏。B151 引入
// `.orch/machine.yaml`（gitignore 承载本机工具路径）+ `resolve_tool_path`
// 作为 argv[0] 的解析层：machine 优先 → project 兜底 → 双缺响亮 Err，
// 绝不裸名 / PATH 猜测（M1）。`machine_config_ignored` 防止 machine.yaml
// 被误入库（M2），`current_md_consistent` 为 `orch doctor` 的 CURRENT.md
// 一致性检查提供判定（M3）。

/// `.orch/machine.yaml` 的本机工具路径表（薄层：只解析 `tools` 段，未知字段容忍）。
///
/// 文件缺失视为本机无覆盖（返回空表，调用方回退 PROJECT-BINDING 默认）。
/// 解析失败响亮报错（不静默兜底——那是 M1 的退化形态）。
#[derive(Debug, Default, Deserialize)]
pub struct MachineConfig {
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub storage: StorageMachineConfig,
}

/// Machine-local storage policy. `storage` applies these values as
/// `max(compiled_default, configured)`, so an overlay can tighten but never relax safety.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageMachineConfig {
    pub floor_bytes: Option<u64>,
    pub audit_reserve_bytes: Option<u64>,
    pub dispatch_estimate_bytes: Option<u64>,
    pub wake_estimate_bytes: Option<u64>,
    pub gate_estimate_bytes: Option<u64>,
}

/// 读取 `.orch/machine.yaml`。文件不存在 → `Ok(None)`（无本机覆盖）。
pub fn load_machine_config(root: &Path) -> Result<Option<MachineConfig>> {
    let p = root.join(".orch/machine.yaml");
    if !p.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&p)
        .with_context(|| format!("读取 machine config 失败: {}", p.display()))?;
    let cfg: MachineConfig = serde_yaml::from_str(&text)
        .with_context(|| format!("解析 machine config 失败: {}", p.display()))?;
    Ok(Some(cfg))
}

/// 解析某工具的可执行路径：machine config 优先 → project 默认兜底 → 双缺响亮 Err。
///
/// **绝不裸名 / PATH 猜测**：两层都没有声明时返回 Err，而不是回退到
/// `tool.to_string()`（M1 退化形态：换机器即静默坏）。`tool` 是稳定 key
/// （如 `cargo`），`project_default` 是 PROJECT-BINDING.yaml gate argv[0]
/// 的当前值（移植前是本机绝对路径，移植后是可移植默认如 `cargo`）。
pub fn resolve_tool_path(
    machine: Option<&str>,
    project_default: Option<&str>,
    tool: &str,
) -> std::result::Result<String, String> {
    if let Some(p) = machine {
        if !p.trim().is_empty() {
            return Ok(p.to_string());
        }
    }
    if let Some(p) = project_default {
        if !p.trim().is_empty() {
            return Ok(p.to_string());
        }
    }
    Err(format!(
        "工具 {tool} 既未在 .orch/machine.yaml 声明也无 PROJECT-BINDING 默认；拒绝裸名/PATH 猜测（B151 M1）"
    ))
}

/// 从根 + 工具名解析：先查 machine config，缺失则用 binding argv[0] 兜底。
///
/// gate/adapter 调用点改经本函数（B151 §2）。`command` 是 PROJECT-BINDING
/// 的命令名（如 `testFast`），用于查 argv[0] 作为 project_default。
pub fn resolve_command_tool(root: &Path, command: &str, tool: &str) -> Result<String> {
    let machine = load_machine_config(root)?;
    let machine_path = machine
        .as_ref()
        .and_then(|cfg| cfg.tools.get(tool))
        .map(String::as_str);
    let binding = load(root)?;
    let project_default = binding
        .commands
        .get(command)
        .and_then(|spec| spec.argv.first())
        .map(String::as_str);
    resolve_tool_path(machine_path, project_default, tool).map_err(anyhow::Error::msg)
}

/// 判定 `.gitignore` 是否含 `.orch/machine.yaml` 行（M2：机器配置必须被忽略，
/// 防止本机路径再次泄漏进库）。逐行 trim 比较（与 doctor 的 `file_contains_line`
/// 同语义，便于测试）。
pub fn machine_config_ignored(gitignore_src: &str) -> bool {
    gitignore_src
        .lines()
        .any(|l| l.trim() == ".orch/machine.yaml")
}

/// 判定 CURRENT.md 与活轮 + main SHA 标记是否一致（M3）。
///
/// 一致 = CURRENT.md 文本同时含 `round: <round>` 与 `main: <main_sha>` 行。
/// `main_sha` 允许短前缀（`orch current` 生成短 SHA，与 BOARD/ledger 的短
/// 形态一致）。`round` 必须精确匹配。
pub fn current_md_consistent(current_src: &str, round: &str, main_sha: &str) -> bool {
    let has_round = current_src
        .lines()
        .map(|l| l.trim())
        .any(|l| l == format!("round: {round}"));
    let has_main = current_src
        .lines()
        .map(|l| l.trim())
        .any(|l| l.starts_with("main:") && l[5..].trim().starts_with(main_sha));
    has_round && has_main
}

#[cfg(test)]
mod b151_tests {
    use super::*;

    // B151 scratch 纪律（同 B106）：scratch 必须落在调用 worktree 的
    // `orch/target/test-tmp`，目录名含 pid + 模块级 AtomicU64 fetch_add 序号
    // （线程安全、无时钟依赖）。**禁止 std::env::temp_dir() / /tmp**（硬禁令）。
    static B151_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn b151_scratch_dir(tag: &str) -> std::path::PathBuf {
        let orch_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let dir = orch_root.join("target/test-tmp").join(format!(
            "b151-{tag}-{}-{}",
            std::process::id(),
            B151_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_command_tool_machine_overrides_project() {
        // machine.yaml 优先于 PROJECT-BINDING argv[0]。
        let tmp = b151_scratch_dir("resolve-cmd-machine");
        std::fs::create_dir_all(tmp.join(".orch")).unwrap();
        std::fs::create_dir_all(tmp.join("coordination")).unwrap();
        std::fs::write(
            tmp.join(".orch/machine.yaml"),
            "tools:\n  cargo: /opt/machine/cargo\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("coordination/PROJECT-BINDING.yaml"),
            "project:\n  ecosystems: [rust]\ncommands:\n  testFast:\n    argv: [cargo, test]\n",
        )
        .unwrap();
        let resolved = resolve_command_tool(&tmp, "testFast", "cargo").unwrap();
        assert_eq!(resolved, "/opt/machine/cargo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_command_tool_falls_back_to_binding_default() {
        // 无 machine.yaml：回退 PROJECT-BINDING argv[0]（可移植默认 cargo）。
        let tmp = b151_scratch_dir("resolve-cmd-fallback");
        std::fs::create_dir_all(tmp.join("coordination")).unwrap();
        std::fs::write(
            tmp.join("coordination/PROJECT-BINDING.yaml"),
            "project:\n  ecosystems: [rust]\ncommands:\n  testFast:\n    argv: [cargo, test]\n",
        )
        .unwrap();
        let resolved = resolve_command_tool(&tmp, "testFast", "cargo").unwrap();
        assert_eq!(resolved, "cargo");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_command_tool_refuses_when_neither_machine_nor_binding() {
        // 命令名缺失 argv → 双缺，响亮 Err（M1 退化形态拒绝）。
        let tmp = b151_scratch_dir("resolve-cmd-refuse");
        std::fs::create_dir_all(tmp.join("coordination")).unwrap();
        std::fs::write(
            tmp.join("coordination/PROJECT-BINDING.yaml"),
            "project:\n  ecosystems: [rust]\ncommands:\n  otherGate:\n    argv: [sh, -c, exit 0]\n",
        )
        .unwrap();
        let err = resolve_command_tool(&tmp, "testFast", "cargo").unwrap_err();
        assert!(err.to_string().contains("cargo"), "{err}");
        assert!(err.to_string().contains("拒绝"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
