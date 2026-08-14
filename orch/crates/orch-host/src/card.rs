//! 任务卡 frontmatter 解析（design/02 §2）。未知字段容忍；正文原样带回（供 prompt 渲染）。

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct CardMeta {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(default)]
    pub round: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(rename = "seedProtocol", default)]
    pub seed_protocol: Option<String>,
    /// Declared seeded-red failure shape. Legacy cards omit this field; new
    /// schema-v2 oracle events require the closed `compile|assertion` enum at
    /// the `round seed-verified` boundary.
    #[serde(rename = "redForm", default)]
    pub red_form: Option<String>,
    #[serde(default)]
    pub seeds: Vec<SeedSpec>,
    #[serde(rename = "writeSet", default)]
    pub write_set: Vec<String>,
    #[serde(rename = "frozenPaths", default)]
    pub frozen_paths: Vec<String>,
    /// Production modules through which this card's acceptance contract is
    /// actually exercised.  Plan requires every entry point to be writable
    /// and not frozen, preventing a card from authorizing only dead-end helper
    /// code while freezing the real integration surface.
    #[serde(rename = "entryPoints", default)]
    pub entry_points: Vec<String>,
    #[serde(default)]
    pub gates: CardGates,
    #[serde(default)]
    pub budgets: CardBudgets,
    #[serde(rename = "requiredReviews", default)]
    pub required_reviews: Vec<RequiredReview>,
    #[serde(rename = "requiredEvidence", default)]
    pub required_evidence: Vec<String>,
    /// One-shot, signed-IR authorization for a self-bootstrap attempt whose
    /// dispatch/collect necessarily predates the first production validation.
    #[serde(rename = "bootstrapPreSignoffAttempt", default)]
    pub bootstrap_pre_signoff_attempt: Option<String>,
    /// Optional complexity tier declared by the card (B141). Parsed by
    /// [`complexity_tier`] into the closed [`ComplexityTier`] enum; absent on
    /// every legacy card so replay stays zero-migration.
    #[serde(default)]
    pub complexity: Option<String>,
    /// Optional intra-round dependency edges declared by the card (B141). The
    /// plan phase builds one whole-round graph from these and refuses cycles
    /// and edges onto cards that are absent from the round.
    #[serde(rename = "dependsOn", default)]
    pub depends_on: Vec<String>,
    /// Optional capability requirements declared by the card (B141). The plan
    /// phase proves the declared set is a subset of the assigned agent's
    /// scheduling roles before the IR is authorized.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SeedSpec {
    pub src: String,
    pub target: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CardGates {
    #[serde(default)]
    pub fast: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CardBudgets {
    #[serde(rename = "wallMinutes", default)]
    pub wall_minutes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredReview {
    pub role: String,
    pub agent: String,
}

pub struct Card {
    pub meta: CardMeta,
    pub body: String,
    pub rel_path: String,
}

/// Closed complexity tier declared by a task card (B141).
///
/// Unknown values fail closed at parse time rather than being silently mapped
/// to a default tier; the mutation that would do that is the seeded-red M1
/// case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComplexityTier {
    Complex,
    Medium,
    Light,
}

/// Parse a declared complexity string into the closed [`ComplexityTier`] enum.
///
/// `complex`, `medium`, and `light` are the only accepted spellings (lower
/// case). Any other value, including the empty string, fails closed so the
/// plan phase can never silently degrade a card's declared complexity into a
/// default tier.
pub fn complexity_tier(value: &str) -> Result<ComplexityTier, String> {
    match value {
        "complex" => Ok(ComplexityTier::Complex),
        "medium" => Ok(ComplexityTier::Medium),
        "light" => Ok(ComplexityTier::Light),
        _ => Err(format!("complexity 值非闭枚举: {value:?}")),
    }
}

pub fn validate_task_id(task_id: &str) -> Result<()> {
    let valid = !task_id.is_empty()
        && task_id
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && task_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    if !valid {
        bail!("taskId 必须是单一安全 component [A-Za-z0-9][A-Za-z0-9-]*: {task_id:?}");
    }
    Ok(())
}

/// 解析 `---\n<yaml>\n---\n<body>` 形状的任务卡
pub fn load(root: &Path, round: &str, task_id: &str) -> Result<Card> {
    validate_task_id(task_id)?;
    let rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let p = root.join(&rel);
    let text =
        fs::read_to_string(&p).with_context(|| format!("读取任务卡失败: {}", p.display()))?;
    parse(&rel, task_id, &text)
}

/// Parse one already-captured immutable task-card byte snapshot.  Plan source
/// fingerprinting uses this entry point so semantic compilation and the bound
/// SHA cannot come from two different reads (ABA-safe).
pub fn parse(rel: &str, task_id: &str, text: &str) -> Result<Card> {
    validate_task_id(task_id)?;
    let rest = text
        .strip_prefix("---")
        .with_context(|| "任务卡缺 frontmatter 起始 ---")?;
    let end = rest
        .find("\n---")
        .context("任务卡缺 frontmatter 结束 ---")?;
    let (fm, body) = rest.split_at(end);
    let meta: CardMeta =
        serde_yaml::from_str(fm).with_context(|| format!("任务卡 frontmatter 解析失败: {rel}"))?;
    if meta.task_id != task_id {
        bail!("任务卡 taskId={} 与请求 {task_id} 不符", meta.task_id);
    }
    Ok(Card {
        meta,
        body: body.trim_start_matches("---").trim_start().to_string(),
        rel_path: rel.to_string(),
    })
}

/// writeSet/frozenPaths 匹配：精确路径，或 `dir/**` 前缀通配
pub fn path_matches(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| {
        if let Some(prefix) = p.strip_suffix("/**") {
            path.starts_with(&format!("{prefix}/")) || path == prefix
        } else {
            path == p
        }
    })
}

/// Require a card to name at least one real production entry point and prove
/// every named entry point is authorized by `writeSet` and excluded from
/// `frozenPaths`.
pub fn entry_points_writable(
    entry_points: &[String],
    write_set: &[String],
    frozen_paths: &[String],
) -> Result<(), String> {
    if entry_points.is_empty() {
        return Err("entryPoints 不得为空".to_string());
    }

    let mut errors = Vec::new();
    for entry_point in entry_points {
        if !path_matches(write_set, entry_point) {
            errors.push(format!("entryPoint 不在 writeSet: {entry_point}"));
        }
        let frozen = path_matches(frozen_paths, entry_point)
            || frozen_paths
                .iter()
                .any(|frozen| path_matches(std::slice::from_ref(entry_point), frozen));
        if frozen {
            errors.push(format!("entryPoint 命中 frozenPaths: {entry_point}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        errors.sort();
        errors.dedup();
        Err(errors.join("; "))
    }
}
