//! 任务卡 frontmatter 解析（design/02 §2）。未知字段容忍；正文原样带回（供 prompt 渲染）。

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::legacy::{NongateSeat, RequiredReview, ReviewFallback, ReviewQuorumPolicy};

#[derive(Debug, Clone, Deserialize)]
pub struct CardMeta {
    /// Explicit task-card generation. Legacy cards omit it; schema 3 cards
    /// must carry the exact integer `3` and use the closed v3 shape.
    #[serde(rename = "schemaVersion", default)]
    pub schema_version: Option<u32>,
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
    /// Role-keyed formal fallback declarations carried beside the frozen
    /// `RequiredReview` shape so historical full struct literals stay valid.
    #[serde(rename = "reviewFallbacks", default)]
    pub review_fallbacks: Vec<ReviewFallback>,
    /// Explicit nongate review obligations. Capacity roles remain admission
    /// qualifications and never create an obligation by themselves.
    #[serde(rename = "nongateSeats", default)]
    pub nongate_seats: Vec<NongateSeat>,
    /// Optional closed quorum policy. Its absence preserves the historical
    /// rule that every formal review in `requiredReviews` is mandatory.
    #[serde(rename = "reviewQuorum", default)]
    pub review_quorum: Option<ReviewQuorumPolicy>,
    /// Task-local authorization for one exact default-primary PASS to satisfy
    /// quorum after every signed formal seat has reached a terminal state.
    /// Missing legacy card fields remain false.
    #[serde(rename = "primaryPassAloneSatisfies", default)]
    pub primary_pass_alone_satisfies: bool,
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
    /// Signed authorization requests for the one permitted mutation of an
    /// already-landed frozen contract.  The complete card bytes are bound by
    /// `ROUND-IR.sourceBindings.taskCards`; keeping this declaration closed
    /// prevents a misspelled anchor from silently disappearing during parse.
    #[serde(rename = "frozenContractSupersessions", default)]
    pub frozen_contract_supersessions: Vec<FrozenContractSupersession>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CardMetaV3 {
    schema_version: u32,
    task_id: String,
    round: String,
    seed_protocol: String,
    red_form: String,
    depends_on: Vec<String>,
    entry_points: Vec<String>,
    seeds: Vec<SeedSpecV3>,
    write_set: Vec<String>,
    frozen_paths: Vec<String>,
    gates: CardGatesV3,
    required_evidence: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedSpecV3 {
    src: String,
    target: String,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct CardGatesV3 {
    fast: Vec<String>,
}

impl CardMetaV3 {
    fn into_compat(self) -> Result<CardMeta> {
        if self.schema_version != 3 {
            bail!(
                "schema 3 task card 的 schemaVersion 必须精确为 3，实得 {}",
                self.schema_version
            );
        }
        Ok(CardMeta {
            schema_version: Some(3),
            task_id: self.task_id,
            round: Some(self.round),
            agent: None,
            seed_protocol: Some(self.seed_protocol),
            red_form: Some(self.red_form),
            seeds: self
                .seeds
                .into_iter()
                .map(|seed| SeedSpec {
                    src: seed.src,
                    target: seed.target,
                    sha256: Some(seed.sha256),
                })
                .collect(),
            write_set: self.write_set,
            frozen_paths: self.frozen_paths,
            entry_points: self.entry_points,
            gates: CardGates {
                fast: self.gates.fast,
            },
            budgets: CardBudgets::default(),
            required_reviews: Vec::new(),
            review_fallbacks: Vec::new(),
            nongate_seats: Vec::new(),
            review_quorum: None,
            primary_pass_alone_satisfies: false,
            required_evidence: self.required_evidence,
            bootstrap_pre_signoff_attempt: None,
            complexity: None,
            depends_on: self.depends_on,
            capabilities: Vec::new(),
            frozen_contract_supersessions: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
/// One signed request to replace exactly one landed frozen-contract digest.
///
/// Historical recovery cards carry the two top-level recovery anchors. New
/// planner-adjudicated cards omit both anchors and carry [`Self::authorization`].
/// The byte-change declaration is also a closed union: historical cards keep
/// the three literal-swap keys at their original top-level locations, while a
/// structured evolution carries only [`Self::structured_evolution`]. Semantic
/// validation requires exactly one complete authorization arm and exactly one
/// complete evolution arm, preserving old YAML while rejecting mixed shapes.
pub struct FrozenContractSupersession {
    pub target: String,
    pub initiator: String,
    pub original_seed_relocated: FrozenSeedRelocationAnchor,
    pub effective_anchor: FrozenEffectiveAnchor,
    pub old_file_sha256: String,
    pub new_file_sha256: String,
    /// Old quoted digest for the historical literal-swap shape. This field is
    /// present together with `newLiteralSha256` and `subjectPrefix`, or all
    /// three fields are absent for a structured evolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_literal_sha256: Option<String>,
    /// New quoted digest for the historical literal-swap shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_literal_sha256: Option<String>,
    /// Subject-prefix binding retained byte-for-byte by the literal shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_prefix: Option<FrozenSubjectPrefix>,
    /// Ordered old-coordinate edits for the structured-evolution shape. It is
    /// mutually exclusive with all three historical literal-swap fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_evolution: Option<FrozenStructuredEvolution>,
    /// Historical recovery anchor for the attempt that proved the old
    /// contract impossible. It is present only for the recovery arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_attempt: Option<FrozenBlockedAttempt>,
    /// Historical recovery anchor for the replacement task that reached
    /// `TaskRecorded`. It is present only for the recovery arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<FrozenRecordedReplacement>,
    /// User-backed planner adjudication used instead of recovery anchors.
    /// It is absent from every historical recovery card and payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<FrozenPlannerAdjudicatedAuthorization>,
    pub dispositions: Vec<FrozenContractDisposition>,
    pub reviews: Vec<RequiredReview>,
}

/// Closed, validated view of a frozen-contract byte-change declaration.
///
/// Callers must obtain this view through [`FrozenContractSupersession::evolution`]
/// instead of attempting fallback between the wire-level optional fields.
#[derive(Debug, Clone, Copy)]
pub enum FrozenContractEvolution<'a> {
    /// Historical shape: exactly one quoted SHA-256 literal changes and its
    /// subject-prefix digest changes by the same pair.
    LiteralSwap {
        /// Digest present in the old quoted literal.
        old_literal_sha256: &'a str,
        /// Digest present in the new quoted literal.
        new_literal_sha256: &'a str,
        /// Production prefix whose old/new digest is the literal pair.
        subject_prefix: &'a FrozenSubjectPrefix,
    },
    /// Structured shape: every changed byte is reconstructed from ordered,
    /// digest-bound edit units expressed against immutable old coordinates.
    Structured(&'a FrozenStructuredEvolution),
}

impl FrozenContractSupersession {
    /// Select exactly one complete evolution shape without fallback.
    ///
    /// A partial literal arm, both arms, or neither arm is rejected before any
    /// snapshot-specific validation can run. In particular, a failed literal
    /// validation is never retried as structured evolution.
    pub fn evolution(&self) -> Result<FrozenContractEvolution<'_>, String> {
        match (
            self.old_literal_sha256.as_deref(),
            self.new_literal_sha256.as_deref(),
            self.subject_prefix.as_ref(),
            self.structured_evolution.as_ref(),
        ) {
            (Some(old), Some(new), Some(subject), None) => {
                Ok(FrozenContractEvolution::LiteralSwap {
                    old_literal_sha256: old,
                    new_literal_sha256: new,
                    subject_prefix: subject,
                })
            }
            (None, None, None, Some(structured)) => {
                Ok(FrozenContractEvolution::Structured(structured))
            }
            _ => Err(
                "frozenContractSupersessions literal shape 与 structuredEvolution 必须互斥且各自完整"
                    .to_string(),
            ),
        }
    }
}

/// Structured old-to-new evolution whose units are interpreted in immutable
/// old-file byte coordinates and applied in declared order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenStructuredEvolution {
    /// Non-empty, strictly ordered, non-overlapping edit units. Runtime
    /// validation also rejects a single whole-file catch-all unit.
    pub units: Vec<FrozenContractEditUnit>,
}

/// One digest-bound edit of an exact byte window in the old frozen contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenContractEditUnit {
    /// Half-open byte range in the immutable old contract.
    pub old_range: FrozenContractByteRange,
    /// SHA-256 of the exact old bytes named by [`Self::old_range`].
    pub old_sha256: String,
    /// Closed replacement-or-deletion action applied to the old window.
    pub edit: FrozenContractEdit,
}

/// Half-open byte interval relative to the immutable old frozen contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenContractByteRange {
    /// Inclusive byte offset in the old contract.
    pub start: u64,
    /// Exclusive byte offset in the old contract.
    pub end: u64,
}

/// Closed action carried by one structured edit unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum FrozenContractEdit {
    /// Replace the old byte window with exact UTF-8 content.
    Replace {
        /// Exact bytes inserted into the reconstructed contract.
        content: String,
    },
    /// Delete the old byte window and bind it to one adjudicated removed
    /// assertion, whose retained coverage is checked independently.
    Delete {
        /// Exact `removedAssertions[].assertion` identity for this deletion.
        #[serde(rename = "removedAssertion")]
        removed_assertion: String,
    },
}

/// Closed authorization evidence for a planner-adjudicated supersession.
///
/// The shape alone is not sufficient authorization: the landed-seed oracle
/// must also read the adjudication blob and assertion mapping from the fixed
/// candidate tree before the declaration may become Effective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenPlannerAdjudicatedAuthorization {
    /// Closed discriminator. The only accepted value is
    /// `planner-adjudicated`.
    pub kind: String,
    /// Candidate-tree document whose bytes explain and authorize the change.
    pub adjudication: FrozenAdjudicationEvidence,
    /// User statement that prevents a planner from self-authorizing the arm.
    pub user_authorization: FrozenUserAuthorization,
    /// Exact old assertions removed and the narrower coverage retained for
    /// each one in the new frozen-contract text.
    pub removed_assertions: Vec<FrozenRemovedAssertion>,
}

/// Immutable candidate-tree binding for the planner's adjudication document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenAdjudicationEvidence {
    /// Canonical repository-relative path immediately under the signed
    /// round's `planning/` directory.
    pub path: String,
    /// Lowercase SHA-256 of the exact regular blob in the candidate tree.
    pub sha256: String,
}

/// User-origin authorization retained verbatim in the signed declaration and
/// its durable `FrozenContractSuperseded` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenUserAuthorization {
    /// Closed actor identity. The only accepted value is `user`.
    pub actor: String,
    /// Non-empty date supplied with the user's decision.
    pub date: String,
    /// Non-empty excerpt that makes the decision auditable without granting
    /// the planner authority to manufacture a sign-off.
    pub quote: String,
}

/// One mechanically checkable old-to-new assertion disposition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenRemovedAssertion {
    /// Exact identifier text that must exist in the old frozen contract and
    /// be absent from the new one.
    pub assertion: String,
    /// Non-empty explanation for removing this specific assertion.
    pub reason: String,
    /// Exact narrower marker that must remain in the new frozen contract.
    pub retained_coverage: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenSeedRelocationAnchor {
    pub event_id: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenEffectiveAnchor {
    /// Closed values: `seed-relocated`, `migration-baseline`,
    /// `migration-tombstone`, or `frozen-contract-superseded`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_tree_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenSubjectPrefix {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenBlockedAttempt {
    pub round: String,
    pub task_id: String,
    pub attempt_id: String,
    pub event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenRecordedReplacement {
    pub round: String,
    pub task_id: String,
    pub task_recorded_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenContractDisposition {
    pub old_assertion: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_assertion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exemption_reason: Option<String>,
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
    let raw: serde_yaml::Value = serde_yaml::from_str(fm)
        .with_context(|| format!("任务卡 frontmatter 预解析失败: {rel}"))?;
    let mapping = raw
        .as_mapping()
        .with_context(|| format!("任务卡 frontmatter 必须是 mapping: {rel}"))?;
    let schema_key = serde_yaml::Value::String("schemaVersion".to_string());
    let schema_version = mapping.get(&schema_key).map(|value| {
        value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .with_context(|| format!("任务卡 schemaVersion 必须是 u32 integer: {rel}"))
    });
    let meta = match schema_version.transpose()? {
        Some(3) => serde_yaml::from_str::<CardMetaV3>(fm)
            .with_context(|| format!("schema 3 任务卡 frontmatter 非 closed shape: {rel}"))?
            .into_compat()?,
        Some(1 | 2) | None => crate::legacy::decode_card_meta_v1_v2(fm, rel)?,
        Some(version) => bail!("任务卡 schemaVersion={version} 未建模: {rel}"),
    };
    if meta.task_id != task_id {
        bail!("任务卡 taskId={} 与请求 {task_id} 不符", meta.task_id);
    }
    validate_frozen_contract_supersessions(&meta).map_err(anyhow::Error::msg)?;
    Ok(Card {
        meta,
        body: body.trim_start_matches("---").trim_start().to_string(),
        rel_path: rel.to_string(),
    })
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

fn nonempty(value: &str) -> bool {
    !value.is_empty() && value.trim() == value
}

fn canonical_round_id(value: &str) -> bool {
    value.strip_prefix('r').is_some_and(|number| {
        !number.is_empty()
            && !number.starts_with('0')
            && number.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn canonical_task_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
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

fn canonical_attempt_id(task_id: &str, attempt_id: &str) -> bool {
    attempt_id
        .strip_prefix(task_id)
        .and_then(|suffix| suffix.strip_prefix("-A"))
        .is_some_and(|number| {
            number.len() == 4
                && number != "0000"
                && number.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn planner_adjudication_round(path: &str) -> Option<&str> {
    canonical_repo_relative(path)?;
    let mut parts = path.split('/');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (
            Some("coordination"),
            Some("rounds"),
            Some(round),
            Some("planning"),
            Some(file_name),
            None,
        ) if canonical_round_id(round) && nonempty(file_name) => Some(round),
        _ => None,
    }
}

fn validate_structured_evolution_schema(
    evolution: &FrozenStructuredEvolution,
) -> Result<BTreeSet<String>, String> {
    if evolution.units.is_empty() {
        return Err("frozenContractSupersessions structuredEvolution.units 不得为空".to_string());
    }
    let mut previous: Option<FrozenContractByteRange> = None;
    let mut deletions = BTreeSet::new();
    for unit in &evolution.units {
        let range = unit.old_range;
        if range.start > range.end {
            return Err(
                "frozenContractSupersessions structuredEvolution oldRange 必须是正向半开区间"
                    .to_string(),
            );
        }
        if let Some(previous) = previous {
            if range.start <= previous.start || range.start < previous.end {
                return Err(
                    "frozenContractSupersessions structuredEvolution units 必须严格有序且互不重叠"
                        .to_string(),
                );
            }
        }
        if !canonical_sha256(&unit.old_sha256) {
            return Err(
                "frozenContractSupersessions structuredEvolution oldSha256 非 canonical SHA-256"
                    .to_string(),
            );
        }
        match &unit.edit {
            FrozenContractEdit::Replace { content } => {
                if content.is_empty() {
                    return Err(
                        "frozenContractSupersessions structuredEvolution replace content 不得为空；删除必须使用 delete"
                            .to_string(),
                    );
                }
            }
            FrozenContractEdit::Delete { removed_assertion } => {
                if range.start == range.end
                    || !nonempty(removed_assertion)
                    || !deletions.insert(removed_assertion.clone())
                {
                    return Err(
                        "frozenContractSupersessions structuredEvolution delete 必须绑定唯一非空 removedAssertion 与非空 oldRange"
                            .to_string(),
                    );
                }
            }
        }
        previous = Some(range);
    }
    Ok(deletions)
}

/// Validate the semantic completeness of every signed supersession request.
/// Byte/durable/git anchors are revalidated by the runtime immediately before
/// Effective emission; this layer ensures no authorization field can be
/// omitted, duplicated, or replaced by an open-ended YAML extension.
pub fn validate_frozen_contract_supersessions(meta: &CardMeta) -> Result<(), String> {
    let mut targets = BTreeSet::new();
    for declaration in &meta.frozen_contract_supersessions {
        if !targets.insert(declaration.target.as_str()) {
            return Err(format!(
                "frozenContractSupersessions target 重复: {}",
                declaration.target
            ));
        }
        if canonical_repo_relative(&declaration.target).is_none() {
            return Err(format!(
                "frozenContractSupersessions target 非规范仓库相对路径: {}",
                declaration.target
            ));
        }
        if !canonical_task_id(&declaration.initiator) || declaration.initiator != meta.task_id {
            return Err(format!(
                "frozenContractSupersessions initiator 必须是承载声明的 taskId {}",
                meta.task_id
            ));
        }
        for (name, digest) in [
            (
                "originalSeedRelocated.sha256",
                declaration.original_seed_relocated.sha256.as_str(),
            ),
            ("oldFileSha256", declaration.old_file_sha256.as_str()),
            ("newFileSha256", declaration.new_file_sha256.as_str()),
        ] {
            if !canonical_sha256(digest) {
                return Err(format!(
                    "frozenContractSupersessions {name} 非 canonical SHA-256"
                ));
            }
        }
        if declaration.old_file_sha256 == declaration.new_file_sha256 {
            return Err("frozenContractSupersessions old/new 摘要不得相同".to_string());
        }
        let structured_deletions = match declaration.evolution()? {
            FrozenContractEvolution::LiteralSwap {
                old_literal_sha256,
                new_literal_sha256,
                subject_prefix,
            } => {
                if !canonical_sha256(old_literal_sha256) || !canonical_sha256(new_literal_sha256) {
                    return Err(
                        "frozenContractSupersessions literal shape 摘要非 canonical SHA-256"
                            .to_string(),
                    );
                }
                if old_literal_sha256 == new_literal_sha256 {
                    return Err("frozenContractSupersessions old/new 摘要不得相同".to_string());
                }
                if canonical_repo_relative(&subject_prefix.path).is_none()
                    || subject_prefix.bytes == 0
                {
                    return Err(
                        "frozenContractSupersessions subjectPrefix 必须绑定规范路径与正字节数"
                            .to_string(),
                    );
                }
                None
            }
            FrozenContractEvolution::Structured(evolution) => {
                Some(validate_structured_evolution_schema(evolution)?)
            }
        };
        if !canonical_event_id(&declaration.original_seed_relocated.event_id) {
            return Err(
                "frozenContractSupersessions original SeedRelocated eventId 非 canonical ULID"
                    .to_string(),
            );
        }
        let effective = &declaration.effective_anchor;
        match effective.kind.as_str() {
            "seed-relocated" | "frozen-contract-superseded" => {
                if effective
                    .event_id
                    .as_deref()
                    .is_none_or(|value| !canonical_event_id(value))
                    || effective.baseline_tree_sha.is_some()
                    || effective
                        .sha256
                        .as_deref()
                        .is_none_or(|value| !canonical_sha256(value))
                {
                    return Err(format!(
                        "frozenContractSupersessions effectiveAnchor {} 字段形状非法",
                        effective.kind
                    ));
                }
            }
            "migration-baseline" => {
                if effective.event_id.is_some()
                    || effective
                        .baseline_tree_sha
                        .as_deref()
                        .is_none_or(|value| !canonical_commit_sha(value))
                    || effective
                        .sha256
                        .as_deref()
                        .is_none_or(|value| !canonical_sha256(value))
                {
                    return Err(
                        "frozenContractSupersessions migration-baseline anchor 字段形状非法"
                            .to_string(),
                    );
                }
            }
            "migration-tombstone" => {
                return Err(
                    "frozenContractSupersessions 不得授权 tombstone 复活；单 literal swap 必须有 old 文件"
                        .to_string(),
                );
            }
            other => {
                return Err(format!(
                    "frozenContractSupersessions effectiveAnchor.kind 非闭枚举: {other:?}"
                ))
            }
        }
        if effective.sha256.as_deref() != Some(declaration.old_file_sha256.as_str()) {
            return Err(
                "frozenContractSupersessions effectiveAnchor.sha256 必须等于 oldFileSha256"
                    .to_string(),
            );
        }
        match (
            declaration.blocked_attempt.as_ref(),
            declaration.replacement.as_ref(),
            declaration.authorization.as_ref(),
        ) {
            (Some(blocked), Some(replacement), None) => {
                if !canonical_round_id(&blocked.round)
                    || !canonical_task_id(&blocked.task_id)
                    || !canonical_attempt_id(&blocked.task_id, &blocked.attempt_id)
                    || !canonical_event_id(&blocked.event_id)
                    || !canonical_round_id(&replacement.round)
                    || !canonical_task_id(&replacement.task_id)
                    || !canonical_event_id(&replacement.task_recorded_event_id)
                {
                    return Err(
                        "frozenContractSupersessions AttemptBlocked/replacement 锚点格式非法"
                            .to_string(),
                    );
                }
                if structured_deletions
                    .as_ref()
                    .is_some_and(|deletions| !deletions.is_empty())
                {
                    return Err(
                        "frozenContractSupersessions structured delete 必须绑定 planner-adjudicated removedAssertions"
                            .to_string(),
                    );
                }
            }
            (None, None, Some(authorization)) => {
                if authorization.kind != "planner-adjudicated" {
                    return Err(
                        "frozenContractSupersessions authorization.kind 必须为 planner-adjudicated"
                            .to_string(),
                    );
                }
                let Some(path_round) =
                    planner_adjudication_round(&authorization.adjudication.path)
                else {
                    return Err(
                        "frozenContractSupersessions adjudication.path 必须精确位于 coordination/rounds/<round>/planning/"
                            .to_string(),
                    );
                };
                if meta
                    .round
                    .as_deref()
                    .is_some_and(|declared_round| declared_round != path_round)
                {
                    return Err(
                        "frozenContractSupersessions adjudication.path round 与任务卡 round 不一致"
                            .to_string(),
                    );
                }
                if !canonical_sha256(&authorization.adjudication.sha256) {
                    return Err(
                        "frozenContractSupersessions adjudication.sha256 非 canonical SHA-256"
                            .to_string(),
                    );
                }
                if authorization.user_authorization.actor != "user"
                    || !nonempty(&authorization.user_authorization.date)
                    || !nonempty(&authorization.user_authorization.quote)
                {
                    return Err(
                        "frozenContractSupersessions userAuthorization 必须绑定 user 与非空 date/quote"
                            .to_string(),
                    );
                }
                if authorization.removed_assertions.is_empty() {
                    return Err(
                        "frozenContractSupersessions removedAssertions 不得为空".to_string(),
                    );
                }
                let mut assertions = BTreeSet::new();
                for removed in &authorization.removed_assertions {
                    if !nonempty(&removed.assertion)
                        || !nonempty(&removed.reason)
                        || !nonempty(&removed.retained_coverage)
                        || removed.assertion == removed.retained_coverage
                        || !assertions.insert(removed.assertion.clone())
                    {
                        return Err(
                            "frozenContractSupersessions removedAssertions 必须逐条绑定唯一 assertion、reason 与不同的 retainedCoverage"
                                .to_string(),
                        );
                    }
                }
                if structured_deletions
                    .as_ref()
                    .is_some_and(|deletions| deletions != &assertions)
                {
                    return Err(
                        "frozenContractSupersessions structured delete 必须与 removedAssertions 逐条双向绑定"
                            .to_string(),
                    );
                }
            }
            _ => {
                return Err(
                    "frozenContractSupersessions recovery 与 planner-adjudicated 两种授权必须互斥且完整"
                        .to_string(),
                )
            }
        }
        if declaration.dispositions.is_empty() {
            return Err("frozenContractSupersessions dispositions 不得为空".to_string());
        }
        let mut old_assertions = BTreeSet::new();
        for disposition in &declaration.dispositions {
            if !nonempty(&disposition.old_assertion)
                || !old_assertions.insert(disposition.old_assertion.as_str())
            {
                return Err(
                    "frozenContractSupersessions disposition oldAssertion 缺失或重复".to_string(),
                );
            }
            let replacement = disposition
                .replacement_assertion
                .as_deref()
                .is_some_and(nonempty);
            let exemption = disposition
                .exemption_reason
                .as_deref()
                .is_some_and(nonempty);
            if replacement == exemption {
                return Err(
                    "每条 disposition 必须且只能声明 replacementAssertion 或 exemptionReason"
                        .to_string(),
                );
            }
        }
        let declared_reviews = declaration
            .reviews
            .iter()
            .map(|review| (review.role.as_str(), review.agent.as_str()))
            .collect::<BTreeSet<_>>();
        let card_reviews = meta
            .required_reviews
            .iter()
            .map(|review| (review.role.as_str(), review.agent.as_str()))
            .collect::<BTreeSet<_>>();
        // Round-less fragments are used by the compatibility oracle to prove
        // historical YAML shape without inventing reviewer identities. Every
        // runtime-capable declaration has a round and still requires the exact
        // signed primary+secondary pair.
        let roundless_compatibility_fragment = meta.round.is_none()
            && declaration.reviews.is_empty()
            && meta.required_reviews.is_empty();
        if !roundless_compatibility_fragment
            && (declaration.reviews.len() != 2
                || declared_reviews.len() != 2
                || meta.required_reviews.len() != 2
                || card_reviews.len() != 2
                || declaration
                    .reviews
                    .iter()
                    .any(|review| !nonempty(&review.agent))
                || !declared_reviews.iter().any(|(role, _)| *role == "primary")
                || !declared_reviews
                    .iter()
                    .any(|(role, _)| *role == "secondary")
                || declaration.reviews[0].agent == declaration.reviews[1].agent
                || declared_reviews != card_reviews)
        {
            return Err(
                "frozenContractSupersessions reviews 必须精确绑定本卡 primary+secondary 双审"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn canonical_repo_relative(path: &str) -> Option<()> {
    let parsed = Path::new(path);
    if path.is_empty()
        || path.trim() != path
        || parsed.is_absolute()
        || path.contains('\\')
        || path.bytes().any(|byte| byte.is_ascii_control())
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        None
    } else {
        Some(())
    }
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

#[cfg(test)]
mod frozen_contract_supersession_tests {
    use super::*;

    const EVENT_ONE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const EVENT_TWO: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    const EVENT_THREE: &str = "01CX5ZZKBKACTAV9WEVGEMMVRZ";
    const EVENT_FOUR: &str = "01DX5ZZKBKACTAV9WEVGEMMVRZ";

    fn hash(character: char) -> String {
        character.to_string().repeat(64)
    }

    fn valid_source() -> String {
        format!(
            r#"---
taskId: B300
requiredReviews:
  - {{role: primary, agent: executor-one}}
  - {{role: secondary, agent: executor-two}}
frozenContractSupersessions:
  - target: orch/crates/orch-host/tests/old_contract.rs
    initiator: B300
    originalSeedRelocated:
      eventId: {EVENT_ONE}
      sha256: {old_file}
    effectiveAnchor:
      kind: seed-relocated
      eventId: {EVENT_TWO}
      sha256: {old_file}
    oldFileSha256: {old_file}
    newFileSha256: {new_file}
    oldLiteralSha256: {old_literal}
    newLiteralSha256: {new_literal}
    subjectPrefix:
      path: orch/crates/orch-host/src/fusion.rs
      bytes: 4096
    blockedAttempt:
      round: r70
      taskId: B253
      attemptId: B253-A0001
      eventId: {EVENT_THREE}
    replacement:
      round: r71
      taskId: B299
      taskRecordedEventId: {EVENT_FOUR}
    dispositions:
      - oldAssertion: production prefix remains byte-identical
        replacementAssertion: production prefix follows the replacement contract
    reviews:
      - {{role: primary, agent: executor-one}}
      - {{role: secondary, agent: executor-two}}
---
body
"#,
            old_file = hash('a'),
            new_file = hash('b'),
            old_literal = hash('c'),
            new_literal = hash('d'),
        )
    }

    fn valid_card() -> Card {
        parse(
            "coordination/rounds/r99/tasks/B300.md",
            "B300",
            &valid_source(),
        )
        .unwrap()
    }

    #[test]
    fn closed_supersession_schema_accepts_complete_authorization() {
        let card = valid_card();
        assert_eq!(card.meta.frozen_contract_supersessions.len(), 1);
        assert_eq!(card.meta.frozen_contract_supersessions[0].initiator, "B300");
    }

    #[test]
    fn closed_supersession_schema_rejects_unknown_and_missing_fields() {
        let unknown = valid_source().replace(
            "    reviews:\n",
            "    unknownAuthorization: true\n    reviews:\n",
        );
        assert!(parse("card", "B300", &unknown).is_err());

        let missing = valid_source().replace(&format!("    newLiteralSha256: {}\n", hash('d')), "");
        assert!(parse("card", "B300", &missing).is_err());
    }

    #[test]
    fn supersession_semantics_reject_duplicate_path_hash_and_anchor_drift() {
        let mut card = valid_card();
        card.meta
            .frozen_contract_supersessions
            .push(card.meta.frozen_contract_supersessions[0].clone());
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("target 重复"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0].target = "../escape.rs".to_string();
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("相对路径"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0].old_file_sha256 = hash('A');
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("SHA-256"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0]
            .effective_anchor
            .sha256 = Some(hash('e'));
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("oldFileSha256"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0]
            .original_seed_relocated
            .event_id = "forged".to_string();
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("ULID"));
    }

    #[test]
    fn supersession_semantics_require_exact_dual_reviews_and_dispositions() {
        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0].reviews.pop();
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("双审"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0].dispositions[0].exemption_reason =
            Some("also exempt".to_string());
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("只能声明"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0].dispositions[0].replacement_assertion = None;
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("只能声明"));
    }

    #[test]
    fn supersession_semantics_bind_initiator_and_attempt_shapes() {
        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0].initiator = "B301".to_string();
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("taskId"));

        let mut card = valid_card();
        card.meta.frozen_contract_supersessions[0]
            .blocked_attempt
            .as_mut()
            .unwrap()
            .attempt_id = "B253-A1".to_string();
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("锚点格式"));
    }

    #[test]
    fn a_round_bound_supersession_never_uses_the_roundless_review_compatibility_shape() {
        let mut card = valid_card();
        card.meta.round = Some("r99".to_string());
        card.meta.required_reviews.clear();
        card.meta.frozen_contract_supersessions[0].reviews.clear();
        assert!(validate_frozen_contract_supersessions(&card.meta)
            .unwrap_err()
            .contains("双审"));
    }
}
