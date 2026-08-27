//! Resolution of test-to-source reader edges used by the candidate gate lane.
//!
//! The reader graph is project data, not a Rust constant.  Every resolution is
//! anchored to the attempt's immutable policy-base commit and verifies both the
//! signed descriptor and its referenced baseline before considering candidate
//! worktree bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::gitx;

const CANDIDATE_POLICY: &str = "candidate-lanes-v1";
const SOURCE_READER_COMMAND: &str = "sourceReaderClosure";
const SEED_TARGET_COMMAND: &str = "seedTargets";
const R81_RUNTIME_ESCALATION_SHA256: &str =
    "983adb428653f908842e31dbffcefda7b50ec3bd888eff205607c37c6f9bf512";

/// One concrete integration-test target selected by the source-reader graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceReaderTargetV1 {
    /// Descriptor command family used to authorize this derived target.
    pub command_ref: String,
    /// Cargo package containing the integration test.
    pub package: String,
    /// Cargo integration-test stem passed after `--test`.
    pub test: String,
    /// Repository-relative reader file whose edge selected the target.
    pub reader: String,
}

/// Fail-closed result of resolving the source-reader portion of a candidate lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceReaderClosureDecisionV1 {
    /// Every relevant edge is descriptor-bound and convertible to a runnable target.
    Closed {
        /// Stable, de-duplicated runnable targets.
        targets: Vec<SourceReaderTargetV1>,
        /// SHA-256 of the exact committed closure descriptor bytes.
        descriptor_sha256: String,
        /// SHA-256 of the exact committed source-shape baseline bytes.
        base_sha256: String,
    },
    /// An unknown or drifting edge requires the signed fast lane.
    UpgradeToFast {
        /// Deterministic explanation retained by the durable escalation event.
        reason: String,
        /// SHA-256 of the verified closure descriptor, when available.
        descriptor_sha256: String,
        /// SHA-256 of the verified base descriptor, when available.
        base_sha256: String,
    },
}

impl SourceReaderClosureDecisionV1 {
    /// Return whether the graph closed without requiring a fast-lane escalation.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed { .. })
    }

    /// Borrow the deterministic escalation reason, if resolution failed closed.
    pub fn upgrade_reason(&self) -> Option<&str> {
        match self {
            Self::UpgradeToFast { reason, .. } => Some(reason),
            Self::Closed { .. } => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BindingEnvelope {
    #[serde(rename = "runtimePolicies")]
    runtime_policies: RuntimePoliciesEnvelope,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimePoliciesEnvelope {
    schema_version: u32,
    policies: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CandidateLanePolicyV1 {
    schema_version: u32,
    owner_task: String,
    initial_state: String,
    scope: String,
    source_reader_closure: SourceReaderDescriptorPointerV1,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceReaderDescriptorPointerV1 {
    schema_version: u32,
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceReaderDescriptorV1 {
    schema_version: u32,
    base_descriptor: BaseDescriptorPointerV1,
    overlays: Vec<SourceReaderOverlayV1>,
    unknown_edge_disposition: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BaseDescriptorPointerV1 {
    path: String,
    sha256: String,
    reader_map_key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceReaderOverlayV1 {
    reader: String,
    mechanism: String,
    #[serde(default)]
    baseline_path: Option<String>,
    #[serde(default)]
    baseline_key: Option<String>,
    subjects: Vec<String>,
    runnable_target: RunnableTargetV1,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunnableTargetV1 {
    command_ref: String,
    package: String,
    test: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeEscalationManifestV1 {
    schema_version: u32,
    round: String,
    closure_descriptor_sha256: String,
    owner_task: String,
    disposition: String,
    reason: String,
    edges: Vec<RuntimeEscalationEdgeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeEscalationEdgeV1 {
    reader: String,
    subject: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineReaderEdgeV1 {
    target: String,
    mechanism: String,
}

#[derive(Debug, Clone)]
struct ReaderEntry {
    mechanism: String,
    subjects: BTreeSet<String>,
    runnable: SourceReaderTargetV1,
}

#[derive(Clone, Copy)]
enum CandidateSnapshot<'a> {
    Filesystem(&'a Path),
    Tree(&'a str),
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_repo_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn integration_target(reader: &str, command_ref: &str) -> Result<SourceReaderTargetV1> {
    if !canonical_repo_path(reader) {
        bail!("source reader path 非 canonical: {reader:?}");
    }
    let components = Path::new(reader)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ["orch", "crates", package, "tests", file] = components.as_slice() else {
        bail!("source reader 无法转换为 Cargo integration target: {reader}");
    };
    let Some(test) = file.strip_suffix(".rs") else {
        bail!("source reader target 不是 .rs: {reader}");
    };
    if test.is_empty()
        || !test
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("source reader test stem 非安全 component: {reader}");
    }
    Ok(SourceReaderTargetV1 {
        command_ref: command_ref.to_string(),
        package: (*package).to_string(),
        test: test.to_string(),
        reader: reader.to_string(),
    })
}

fn json_key<'a>(value: &'a serde_json::Value, dotted: &str) -> Option<&'a serde_json::Value> {
    dotted
        .split('.')
        .try_fold(value, |current, key| current.get(key))
}

fn validate_mechanism(mechanism: &str) -> bool {
    matches!(
        mechanism,
        "IncludeStr" | "RuntimeRead" | "RuntimeReadExternalBaseline"
    )
}

fn rust_without_token_trivia_with_offsets(source: &str) -> (String, Vec<usize>) {
    let bytes = source.as_bytes();
    let mut compact = Vec::with_capacity(bytes.len());
    let mut source_offsets = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if bytes[cursor..].starts_with(b"//") {
            cursor += 2;
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                cursor += 1;
            }
            continue;
        }
        if bytes[cursor..].starts_with(b"/*") {
            cursor += 2;
            let mut depth = 1usize;
            while cursor < bytes.len() && depth > 0 {
                if bytes[cursor..].starts_with(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if bytes[cursor..].starts_with(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            continue;
        }

        // Keep string contents byte-for-byte so comment markers inside a path cannot
        // consume later code. Raw byte strings share the same `r###"..."###` delimiter.
        let raw_prefix = match bytes[cursor] {
            b'r' => Some((cursor, cursor + 1)),
            b'b' if bytes.get(cursor + 1) == Some(&b'r') => Some((cursor, cursor + 2)),
            _ => None,
        };
        if let Some((start, marker)) = raw_prefix {
            let mut quote = marker;
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                let hashes = quote - marker;
                let mut end = quote + 1;
                while end < bytes.len() {
                    if bytes[end] == b'"'
                        && bytes
                            .get(end + 1..end + 1 + hashes)
                            .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                    {
                        end += 1 + hashes;
                        break;
                    }
                    end += 1;
                }
                compact.extend_from_slice(&bytes[start..end]);
                source_offsets.extend(start..end);
                cursor = end;
                continue;
            }
        }
        if bytes[cursor] == b'"' {
            let start = cursor;
            cursor += 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor = (cursor + 2).min(bytes.len()),
                    b'"' => {
                        cursor += 1;
                        break;
                    }
                    _ => cursor += 1,
                }
            }
            compact.extend_from_slice(&bytes[start..cursor]);
            source_offsets.extend(start..cursor);
            continue;
        }

        compact.push(bytes[cursor]);
        source_offsets.push(cursor);
        cursor += 1;
    }
    // The output is a byte-preserving subsequence of valid UTF-8 source.
    (
        String::from_utf8(compact).expect("Rust source trivia compaction preserves UTF-8"),
        source_offsets,
    )
}

fn rust_without_token_trivia(source: &str) -> String {
    rust_without_token_trivia_with_offsets(source).0
}

fn rust_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn contains_rust_identifier(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .enumerate()
        .any(|(index, seen)| {
            seen == needle
                && index
                    .checked_sub(1)
                    .and_then(|prior| haystack.get(prior))
                    .is_none_or(|byte| !rust_identifier_byte(*byte))
                && haystack
                    .get(index + needle.len())
                    .is_none_or(|byte| !rust_identifier_byte(*byte))
        })
}

fn test_cfg_region(source: &str) -> Option<&str> {
    let (compact, source_offsets) = rust_without_token_trivia_with_offsets(source);
    let bytes = compact.as_bytes();
    let mut attribute_start = 0;
    while attribute_start < bytes.len() {
        let expression_start = if bytes[attribute_start..].starts_with(b"#[cfg(") {
            Some(attribute_start + b"#[cfg(".len())
        } else if bytes[attribute_start..].starts_with(b"#[cfg_attr(") {
            Some(attribute_start + b"#[cfg_attr(".len())
        } else {
            None
        };
        let Some(expression_start) = expression_start else {
            attribute_start += 1;
            continue;
        };

        let mut cursor = expression_start;
        let mut depth = 1usize;
        while cursor < bytes.len() && depth > 0 {
            match bytes[cursor] {
                b'"' => {
                    cursor += 1;
                    while cursor < bytes.len() {
                        match bytes[cursor] {
                            b'\\' => cursor = (cursor + 2).min(bytes.len()),
                            b'"' => {
                                cursor += 1;
                                break;
                            }
                            _ => cursor += 1,
                        }
                    }
                }
                b'(' => {
                    depth += 1;
                    cursor += 1;
                }
                b')' => {
                    depth -= 1;
                    cursor += 1;
                }
                _ => cursor += 1,
            }
        }
        if depth == 0 && contains_rust_identifier(&bytes[expression_start..cursor - 1], b"test") {
            return source_offsets
                .get(attribute_start)
                .map(|offset| &source[*offset..]);
        }
        attribute_start = cursor.max(attribute_start + 1);
    }
    None
}

fn source_contains_reader(source: &str) -> bool {
    let compact = rust_without_token_trivia(source);
    [
        "include_str!",
        "include_bytes!",
        "read_to_string",
        "fs::read",
        "::read(",
        "std::fs",
        "File::open",
        ".open(",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
}

fn candidate_reader_is_unknown(source: &str) -> Option<&'static str> {
    if !source_contains_reader(source) {
        return None;
    }
    let compact = rust_without_token_trivia(source);
    if compact.contains("macro_rules!") {
        return Some("changed reader contains a reader macro");
    }
    if (compact.contains("include_str!")
        || compact.contains("include_bytes!"))
        && compact.contains("concat!")
    {
        return Some("changed reader contains a dynamic include macro");
    }
    None
}

fn direct_test_region(source: &str) -> Option<&str> {
    let (compact, source_offsets) = rust_without_token_trivia_with_offsets(source);
    let mut offset = 0usize;
    while let Some(relative) = compact[offset..].find("#[test]") {
        let occurrence = offset + relative;
        if let Some(source_offset) = source_offsets
            .get(occurrence)
            .copied()
            .filter(|source_offset| rust_offset_is_code(source, *source_offset))
        {
            return Some(&source[source_offset..]);
        }
        offset = occurrence + "#[test]".len();
    }
    None
}

fn quoted_literal_after(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(start) != Some(&b'\"') {
        return None;
    }
    let mut cursor = start + 1;
    let mut value = String::new();
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\"' => return Some((value, cursor + 1)),
            b'\\' => {
                let escaped = *bytes.get(cursor + 1)?;
                match escaped {
                    b'\\' | b'\"' => value.push(char::from(escaped)),
                    _ => return None,
                }
                cursor += 2;
            }
            byte if byte.is_ascii() => {
                value.push(char::from(byte));
                cursor += 1;
            }
            _ => return None,
        }
    }
    None
}

fn rust_offset_is_code(source: &str, target: usize) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < target && cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"//") {
            let end = bytes[cursor + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| cursor + 2 + offset + 1);
            if target < end {
                return false;
            }
            cursor = end;
            continue;
        }
        if bytes[cursor..].starts_with(b"/*") {
            let mut end = cursor + 2;
            let mut depth = 1usize;
            while end < bytes.len() && depth > 0 {
                if bytes[end..].starts_with(b"/*") {
                    depth += 1;
                    end += 2;
                } else if bytes[end..].starts_with(b"*/") {
                    depth -= 1;
                    end += 2;
                } else {
                    end += 1;
                }
            }
            if target < end {
                return false;
            }
            cursor = end;
            continue;
        }

        let raw_start = if bytes[cursor] == b'r' {
            Some(cursor + 1)
        } else if bytes[cursor..].starts_with(b"br") {
            Some(cursor + 2)
        } else {
            None
        };
        if let Some(mut quote) = raw_start {
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'\"') {
                let hashes = quote - raw_start.unwrap();
                let mut end = quote + 1;
                while end < bytes.len() {
                    if bytes[end] == b'\"'
                        && bytes
                            .get(end + 1..end + 1 + hashes)
                            .is_some_and(|seen| seen.iter().all(|byte| *byte == b'#'))
                    {
                        end += 1 + hashes;
                        break;
                    }
                    end += 1;
                }
                if target < end {
                    return false;
                }
                cursor = end;
                continue;
            }
        }

        if bytes[cursor] == b'\"' {
            let mut end = cursor + 1;
            while end < bytes.len() {
                match bytes[end] {
                    b'\\' => end = (end + 2).min(bytes.len()),
                    b'\"' => {
                        end += 1;
                        break;
                    }
                    _ => end += 1,
                }
            }
            if target < end {
                return false;
            }
            cursor = end;
            continue;
        }
        cursor += 1;
    }
    true
}

fn literal_macro_reader_paths(source: &str) -> (Vec<String>, bool) {
    let (compact, source_offsets) = rust_without_token_trivia_with_offsets(source);
    let mut literals = Vec::new();
    let mut dynamic = false;
    for prefix in ["include_str!(", "include_bytes!("] {
        let mut offset = 0usize;
        while let Some(relative) = compact[offset..].find(prefix) {
            let occurrence = offset + relative;
            let value_start = occurrence + prefix.len();
            if !source_offsets
                .get(occurrence)
                .is_some_and(|source_offset| rust_offset_is_code(source, *source_offset))
            {
                offset = value_start;
                continue;
            }
            match quoted_literal_after(&compact, value_start) {
                Some((literal, next)) => {
                    literals.push(literal);
                    offset = next;
                }
                None => {
                    dynamic = true;
                    offset = value_start;
                }
            }
        }
    }
    (literals, dynamic)
}

fn source_contains_unit_source_reader(source: &str) -> bool {
    let (macro_literals, dynamic_macro) = literal_macro_reader_paths(source);
    if dynamic_macro || !macro_literals.is_empty() {
        return true;
    }
    if !source_contains_reader(source) {
        return false;
    }
    let compact = rust_without_token_trivia(source);
    compact.contains("\"../src/")
        || compact.contains("\"orch/crates/")
        || (compact.contains("/src/") && compact.contains(".rs\""))
}

fn normalize_literal_reader_subject(reader: &str, literal: &str) -> Option<String> {
    if Path::new(literal).is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    let joined = Path::new(reader).parent()?.join(literal);
    for component in joined.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let normalized = normalized.to_str()?.replace('\\', "/");
    canonical_repo_path(&normalized).then_some(normalized)
}

fn policy_base_macro_reader_paths(root: &Path, policy_base_sha: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "grep",
            "-z",
            "-l",
            "-F",
            "-e",
            "include",
            policy_base_sha,
            "--",
            ":(glob)orch/crates/**/*.rs",
        ])
        .output()
        .context("git grep policy-base source readers 启动失败")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "git grep policy-base source readers 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let prefix = format!("{policy_base_sha}:");
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            std::str::from_utf8(entry)
                .context("policy-base reader path 非 UTF-8")
                .and_then(|entry| {
                    entry
                        .strip_prefix(&prefix)
                        .context("policy-base reader path 缺 treeish prefix")
                        .map(str::to_string)
                })
        })
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    paths.dedup();
    if paths
        .iter()
        .any(|path| !path.ends_with(".rs") || !canonical_repo_path(path))
    {
        bail!("policy-base reader path 非 canonical Rust source");
    }
    Ok(paths)
}

fn same_crate_source(left: &str, right: &str) -> bool {
    fn crate_prefix(path: &str) -> Option<&str> {
        let mut parts = path.split('/');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("orch"), Some("crates"), Some(package)) => Some(package),
            _ => None,
        }
    }
    crate_prefix(left).is_some_and(|package| crate_prefix(right) == Some(package))
}

fn unknown_policy_base_reader_edge(
    root: &Path,
    policy_base_sha: &str,
    entries: &BTreeMap<String, ReaderEntry>,
    changed: &BTreeSet<String>,
) -> Result<Option<(String, String)>> {
    for reader in policy_base_macro_reader_paths(root, policy_base_sha)? {
        if entries.contains_key(&reader) {
            continue;
        }
        let bytes = gitx::show_bytes(root, policy_base_sha, &reader)?;
        let source = std::str::from_utf8(&bytes)
            .with_context(|| format!("policy-base reader 非 UTF-8: {reader}"))?;
        let (literals, dynamic) = literal_macro_reader_paths(source);
        for literal in literals {
            let Some(subject) = normalize_literal_reader_subject(&reader, &literal) else {
                if changed.iter().any(|subject| same_crate_source(&reader, subject)) {
                    return Ok(Some((reader, literal)));
                }
                continue;
            };
            if changed.contains(&subject) {
                return Ok(Some((reader, subject)));
            }
        }
        if dynamic && changed.iter().any(|subject| same_crate_source(&reader, subject)) {
            let subject = changed
                .iter()
                .find(|subject| same_crate_source(&reader, subject))
                .cloned()
                .context("same-crate unknown reader subject disappeared")?;
            return Ok(Some((reader, subject)));
        }
    }
    Ok(None)
}

fn load_descriptor(
    root: &Path,
    policy_base_sha: &str,
) -> Result<(SourceReaderDescriptorV1, serde_json::Value, String, String)> {
    let binding_bytes =
        gitx::show_bytes(root, policy_base_sha, "coordination/PROJECT-BINDING.yaml")
            .context("读取 policy-base binding 失败")?;
    let binding: BindingEnvelope = serde_yaml::from_slice(&binding_bytes)
        .context("policy-base binding runtimePolicies 非 exact")?;
    if binding.runtime_policies.schema_version != 1 {
        bail!("runtimePolicies schemaVersion 必须为 1");
    }
    let policy_value = binding
        .runtime_policies
        .policies
        .get(CANDIDATE_POLICY)
        .context("binding 缺 candidate-lanes-v1")?;
    let policy: CandidateLanePolicyV1 = serde_yaml::from_value(policy_value.clone())
        .context("candidate-lanes-v1 descriptor 非 exact")?;
    if policy.schema_version != 1
        || policy.owner_task != "B306"
        || !matches!(policy.initial_state.as_str(), "dormant" | "active")
        || policy.scope != "round"
        || policy.source_reader_closure.schema_version != 1
        || !valid_sha256(&policy.source_reader_closure.sha256)
        || !canonical_repo_path(&policy.source_reader_closure.path)
    {
        bail!("candidate-lanes-v1 policy/pointer contract 漂移");
    }

    let descriptor_bytes =
        gitx::show_bytes(root, policy_base_sha, &policy.source_reader_closure.path)
            .context("读取 committed source-reader descriptor 失败")?;
    let descriptor_sha = sha256(&descriptor_bytes);
    if descriptor_sha != policy.source_reader_closure.sha256 {
        bail!("source-reader descriptor SHA 漂移");
    }
    let descriptor: SourceReaderDescriptorV1 = serde_json::from_slice(&descriptor_bytes)
        .context("source-reader descriptor JSON 非 exact")?;
    if descriptor.schema_version != 1
        || descriptor.unknown_edge_disposition != "upgrade-to-fast-and-audit"
        || descriptor.base_descriptor.reader_map_key != "registeredReaders"
        || !canonical_repo_path(&descriptor.base_descriptor.path)
        || !valid_sha256(&descriptor.base_descriptor.sha256)
    {
        bail!("source-reader descriptor envelope 漂移");
    }
    let base_bytes = gitx::show_bytes(root, policy_base_sha, &descriptor.base_descriptor.path)
        .context("读取 committed source-shape baseline 失败")?;
    let base_sha = sha256(&base_bytes);
    if base_sha != descriptor.base_descriptor.sha256 {
        bail!("source-shape base descriptor SHA 漂移");
    }
    let base: serde_json::Value =
        serde_json::from_slice(&base_bytes).context("source-shape baseline 非 JSON")?;
    if base
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || base
            .get(&descriptor.base_descriptor.reader_map_key)
            .and_then(serde_json::Value::as_object)
            .is_none()
    {
        bail!("source-shape baseline schema/reader map 漂移");
    }
    Ok((descriptor, base, descriptor_sha, base_sha))
}

/// Reauthenticate the source-reader descriptor and baseline at one immutable policy base.
///
/// Escalated receipt replay uses this narrow boundary to retain descriptor identity without
/// reclassifying or weakening the attempt's already-monotonic fast-lane decision.
pub(crate) fn authenticated_source_reader_digests_v1(
    root: &Path,
    policy_base_sha: &str,
) -> Result<(String, String)> {
    let (_, _, descriptor_sha256, base_sha256) = load_descriptor(root, policy_base_sha)?;
    Ok((descriptor_sha256, base_sha256))
}

fn build_reader_entries(
    descriptor: &SourceReaderDescriptorV1,
    base: &serde_json::Value,
) -> Result<BTreeMap<String, ReaderEntry>> {
    let reader_map = base
        .get(&descriptor.base_descriptor.reader_map_key)
        .and_then(serde_json::Value::as_object)
        .context("source-shape baseline reader map 非 object")?;
    let mut entries = BTreeMap::<String, ReaderEntry>::new();
    for (reader, edges) in reader_map {
        let parsed: Vec<BaselineReaderEdgeV1> =
            serde_json::from_value(edges.clone()).context("baseline reader edge 非 exact")?;
        if parsed.is_empty() {
            bail!("baseline reader edge 为空: {reader}");
        }
        let mut subjects = BTreeSet::new();
        let mut mechanism = None::<String>;
        for edge in parsed {
            if !canonical_repo_path(&edge.target) || !validate_mechanism(&edge.mechanism) {
                bail!("baseline reader edge 路径/mechanism 非 canonical: {reader}");
            }
            if let Some(seen) = &mechanism {
                if seen != &edge.mechanism {
                    bail!("baseline reader 混用多个 mechanism: {reader}");
                }
            } else {
                mechanism = Some(edge.mechanism.clone());
            }
            subjects.insert(edge.target);
        }
        entries.insert(
            reader.clone(),
            ReaderEntry {
                mechanism: mechanism.context("baseline reader 缺 mechanism")?,
                subjects,
                runnable: integration_target(reader, SOURCE_READER_COMMAND)?,
            },
        );
    }

    for overlay in &descriptor.overlays {
        if !canonical_repo_path(&overlay.reader)
            || !validate_mechanism(&overlay.mechanism)
            || overlay.subjects.is_empty()
            || overlay
                .subjects
                .iter()
                .any(|subject| !canonical_repo_path(subject))
            || !matches!(
                overlay.runnable_target.command_ref.as_str(),
                SOURCE_READER_COMMAND | SEED_TARGET_COMMAND
            )
        {
            bail!("source-reader overlay 非 canonical: {}", overlay.reader);
        }
        match overlay.mechanism.as_str() {
            "RuntimeReadExternalBaseline" => {
                let path = overlay
                    .baseline_path
                    .as_deref()
                    .context("external baseline overlay 缺 baselinePath")?;
                let key = overlay
                    .baseline_key
                    .as_deref()
                    .context("external baseline overlay 缺 baselineKey")?;
                if path != descriptor.base_descriptor.path || json_key(base, key).is_none() {
                    bail!(
                        "external baseline overlay path/key 漂移: {}",
                        overlay.reader
                    );
                }
            }
            _ if overlay.baseline_path.is_some() || overlay.baseline_key.is_some() => {
                bail!("非 external overlay 不得声明 baseline path/key");
            }
            _ => {}
        }
        let derived = integration_target(&overlay.reader, &overlay.runnable_target.command_ref)?;
        if derived.package != overlay.runnable_target.package
            || derived.test != overlay.runnable_target.test
        {
            bail!(
                "overlay runnableTarget 与 reader path 不一致: {}",
                overlay.reader
            );
        }
        let subjects = overlay.subjects.iter().cloned().collect::<BTreeSet<_>>();
        match entries.get_mut(&overlay.reader) {
            Some(existing) => {
                if existing.mechanism != overlay.mechanism
                    || existing.runnable != derived
                    || !existing.subjects.is_disjoint(&subjects)
                {
                    bail!(
                        "source-reader overlay 与 base edge 冲突: {}",
                        overlay.reader
                    );
                }
                existing.subjects.extend(subjects);
            }
            None => {
                entries.insert(
                    overlay.reader.clone(),
                    ReaderEntry {
                        mechanism: overlay.mechanism.clone(),
                        subjects,
                        runnable: derived,
                    },
                );
            }
        }
    }
    Ok(entries)
}

fn load_runtime_escalations(
    root: &Path,
    round: &str,
    policy_base_sha: &str,
    descriptor_sha256: &str,
) -> Result<Option<(RuntimeEscalationManifestV1, String)>> {
    let path =
        format!("coordination/rounds/{round}/planning/source-reader-runtime-escalations.json");
    if !gitx::tree_path_exists(root, policy_base_sha, &path)? {
        if round == "r81" {
            bail!("r81 exact runtime escalation 清单缺失");
        }
        return Ok(None);
    }
    let bytes = gitx::show_bytes(root, policy_base_sha, &path)
        .with_context(|| format!("读取 committed runtime escalation 清单失败: {path}"))?;
    let digest = sha256(&bytes);
    if round == "r81" && digest != R81_RUNTIME_ESCALATION_SHA256 {
        bail!(
            "r81 runtime escalation 清单 SHA 漂移: expected={} actual={digest}",
            R81_RUNTIME_ESCALATION_SHA256
        );
    }
    let manifest: RuntimeEscalationManifestV1 =
        serde_json::from_slice(&bytes).context("runtime escalation 清单 JSON 非 exact")?;
    if manifest.schema_version != 1
        || manifest.round != round
        || manifest.closure_descriptor_sha256 != descriptor_sha256
        || manifest.owner_task.trim().is_empty()
        || manifest.owner_task.trim() != manifest.owner_task
        || manifest.disposition != "upgrade-to-fast-and-audit"
        || manifest.reason.trim().is_empty()
        || manifest.edges.is_empty()
        || manifest
            .edges
            .iter()
            .any(|edge| !canonical_repo_path(&edge.reader) || !canonical_repo_path(&edge.subject))
        || manifest.edges.windows(2).any(|pair| pair[0] >= pair[1])
    {
        bail!("runtime escalation 清单 envelope/edge 非 canonical");
    }
    Ok(Some((manifest, digest)))
}

fn candidate_snapshot_bytes(
    root: &Path,
    snapshot: CandidateSnapshot<'_>,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    match snapshot {
        CandidateSnapshot::Filesystem(candidate_root) => {
            let candidate = candidate_root.join(path);
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    fs::read(&candidate)
                        .map(Some)
                        .with_context(|| format!("读取 candidate source 失败: {path}"))
                }
                Ok(_) => Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    Err(error).with_context(|| format!("检查 candidate source 失败: {path}"))
                }
            }
        }
        CandidateSnapshot::Tree(treeish) => {
            if !gitx::tree_path_exists(root, treeish, path)? {
                return Ok(None);
            }
            gitx::show_bytes(root, treeish, path)
                .map(Some)
                .with_context(|| format!("读取 candidate tree source 失败: {path}"))
        }
    }
}

fn resolve_source_reader_closure_from_snapshot(
    root: &Path,
    snapshot: CandidateSnapshot<'_>,
    round: Option<&str>,
    policy_base_sha: &str,
    changed_paths: &[String],
) -> Result<SourceReaderClosureDecisionV1> {
    let (descriptor, base, descriptor_sha, base_sha) = match load_descriptor(root, policy_base_sha)
    {
        Ok(value) => value,
        Err(error) => {
            return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                reason: format!("source-reader descriptor/base drift: {error:#}"),
                descriptor_sha256: String::new(),
                base_sha256: String::new(),
            });
        }
    };
    let entries = match build_reader_entries(&descriptor, &base) {
        Ok(entries) => entries,
        Err(error) => {
            return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                reason: format!("source-reader graph is not closed: {error:#}"),
                descriptor_sha256: descriptor_sha,
                base_sha256: base_sha,
            });
        }
    };
    let changed = changed_paths.iter().cloned().collect::<BTreeSet<_>>();

    if let Some(round) = round {
        match load_runtime_escalations(root, round, policy_base_sha, &descriptor_sha) {
            Ok(Some((manifest, manifest_sha256))) => {
                if let Some(edge) = manifest.edges.iter().find(|edge| {
                    changed.contains(&edge.subject)
                        && !entries
                            .get(&edge.reader)
                            .is_some_and(|entry| entry.subjects.contains(&edge.subject))
                }) {
                    return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                        reason: format!(
                            "committed runtime escalation {} binds newly-landed unknown reader edge {} -> {}: {}",
                            manifest_sha256, edge.reader, edge.subject, manifest.reason
                        ),
                        descriptor_sha256: descriptor_sha,
                        base_sha256: base_sha,
                    });
                }
            }
            Ok(None) => {}
            Err(error) => {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!("runtime escalation list drift: {error:#}"),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
        }
    }

    match unknown_policy_base_reader_edge(root, policy_base_sha, &entries, &changed) {
        Ok(Some((reader, subject))) => {
            return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                reason: format!(
                    "policy-base reader omitted from source-reader descriptor: {reader} -> {subject}"
                ),
                descriptor_sha256: descriptor_sha,
                base_sha256: base_sha,
            });
        }
        Ok(None) => {}
        Err(error) => {
            return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                reason: format!("policy-base source-reader inventory is not closed: {error:#}"),
                descriptor_sha256: descriptor_sha,
                base_sha256: base_sha,
            });
        }
    }

    for path in &changed {
        if !path.ends_with(".rs") {
            continue;
        }
        let source_bytes = match candidate_snapshot_bytes(root, snapshot, path)? {
            Some(bytes) => bytes,
            None => {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!("changed reader target disappeared or is not regular: {path}"),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
        };
        let source = match std::str::from_utf8(&source_bytes) {
            Ok(source) => source,
            Err(_) => {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!("changed Rust source is not UTF-8: {path}"),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
        };
        if !path.contains("/tests/") {
            // A changed unit-test reader cannot be converted into the runnable integration-test
            // target required by this V1 descriptor. Locate the first cfg expression which names
            // `test` after stripping legal token trivia; exact `#[cfg(test)]` bytes are not an
            // authorization boundary, and compound cfg expressions remain fail-closed.
            let unit_test_reader = [test_cfg_region(source), direct_test_region(source)]
                .into_iter()
                .flatten()
                .any(source_contains_unit_source_reader);
            if unit_test_reader {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!(
                        "changed unit-test source reader cannot convert to an integration target: {path}"
                    ),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
            continue;
        }
        let contains_reader = source_contains_reader(source);
        if contains_reader && !entries.contains_key(path) {
            return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                reason: format!("changed test declares an unregistered source reader: {path}"),
                descriptor_sha256: descriptor_sha,
                base_sha256: base_sha,
            });
        }
        if entries.contains_key(path) {
            match gitx::show_bytes(root, policy_base_sha, path) {
                Ok(committed) if committed == source_bytes => {}
                Ok(_) => {
                    return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                        reason: format!(
                            "registered reader bytes changed; literal/dynamic edge provenance must be reclassified: {path}"
                        ),
                        descriptor_sha256: descriptor_sha,
                        base_sha256: base_sha,
                    });
                }
                Err(error) => {
                    return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                        reason: format!(
                            "registered reader has no policy-base provenance: {path}: {error:#}"
                        ),
                        descriptor_sha256: descriptor_sha,
                        base_sha256: base_sha,
                    });
                }
            }
        }
        if let Some(reason) = candidate_reader_is_unknown(source) {
            return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                reason: format!("{reason}: {path}"),
                descriptor_sha256: descriptor_sha,
                base_sha256: base_sha,
            });
        }
    }

    let mut targets = BTreeSet::new();
    for (reader, entry) in &entries {
        if entry.subjects.is_disjoint(&changed) {
            continue;
        }
        let candidate_bytes = match candidate_snapshot_bytes(root, snapshot, reader)? {
            Some(bytes) => bytes,
            None => {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!("registered reader is not runnable in candidate: {reader}"),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
        };
        match gitx::show_bytes(root, policy_base_sha, reader) {
            Ok(committed_bytes) if committed_bytes == candidate_bytes => {}
            Ok(_) => {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!(
                        "selected reader bytes differ from policy base and cannot authorize reuse: {reader}"
                    ),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
            Err(error) => {
                return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                    reason: format!("selected reader lacks policy-base bytes: {reader}: {error:#}"),
                    descriptor_sha256: descriptor_sha,
                    base_sha256: base_sha,
                });
            }
        }
        targets.insert(entry.runnable.clone());
    }

    Ok(SourceReaderClosureDecisionV1::Closed {
        targets: targets.into_iter().collect(),
        descriptor_sha256: descriptor_sha,
        base_sha256: base_sha,
    })
}

/// Resolve the candidate's complete reader-test closure from immutable signed data.
///
/// Descriptor/base drift, candidate-specific unknown readers, and missing runnable targets all
/// return [`SourceReaderClosureDecisionV1::UpgradeToFast`], allowing the caller to append the
/// canonical B310 escalation fact before running the separately signed fast lane. Empty digest
/// fields mean the corresponding descriptor could not be authenticated and therefore must never
/// participate in a reuse identity.
pub fn resolve_source_reader_closure_v1(
    root: &Path,
    candidate_root: &Path,
    policy_base_sha: &str,
    changed_paths: &[String],
) -> Result<SourceReaderClosureDecisionV1> {
    resolve_source_reader_closure_from_snapshot(
        root,
        CandidateSnapshot::Filesystem(candidate_root),
        None,
        policy_base_sha,
        changed_paths,
    )
}

/// Replay the complete reader-test closure against an immutable candidate commit.
///
/// This is the receipt-safe entry used after the detached gate worktree has been removed. It reads
/// candidate bytes from `candidate_sha`, consumes a strict round-scoped committed escalation list
/// when present, and otherwise applies the same descriptor/base checks as the filesystem entry.
/// A newly landed reader omitted by the signed descriptor can therefore only upgrade to fast; the
/// supplemental list never grants it a narrow runnable target.
pub fn resolve_source_reader_closure_at_tree_v1(
    root: &Path,
    round: &str,
    candidate_sha: &str,
    policy_base_sha: &str,
    changed_paths: &[String],
) -> Result<SourceReaderClosureDecisionV1> {
    resolve_source_reader_closure_from_snapshot(
        root,
        CandidateSnapshot::Tree(candidate_sha),
        Some(round),
        policy_base_sha,
        changed_paths,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_reader_conversion_is_closed_and_exact() {
        let target = integration_target(
            "orch/crates/orch-host/tests/attempt_body_relocation.rs",
            SEED_TARGET_COMMAND,
        )
        .unwrap();
        assert_eq!(target.package, "orch-host");
        assert_eq!(target.test, "attempt_body_relocation");
        assert_eq!(target.command_ref, SEED_TARGET_COMMAND);
    }

    #[test]
    fn nested_or_dynamic_reader_targets_fail_closed() {
        assert!(integration_target(
            "orch/crates/orch-host/tests/support/mod.rs",
            SOURCE_READER_COMMAND,
        )
        .is_err());
        assert!(candidate_reader_is_unknown(
            "macro_rules! reader { () => { include_str!(concat!(\"../src/\", X)) } }"
        )
        .is_some());
    }
}
