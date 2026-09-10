//! Read-only compatibility boundary for closed schema 1/2 rounds.
//!
//! B323 moves legacy Formal/Panel/Quorum decoders and audits behind this
//! module. Schema 3 live writers must never enter this boundary.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use crate::ledger::{
    canonical_runtime_event_v1, decode_runtime_event_v1, unresolved_review_panels_v1,
    validate_runtime_event_history_v1, validate_runtime_event_history_v1_at_root,
    RuntimeEventPayloadV1,
};
pub use crate::plan::{
    resolve_attempt_runtime_policy, resolve_runtime_policy_at,
    resolved_command_argv_digest_at_policy_base,
};
pub use crate::verify::{
    evaluate_review_quorum, evaluate_review_quorum_for_task, review_contract_mode_for_attempt,
    validate_archived_record_chain, ReviewContractMode, ReviewQuorumDecision, ReviewResult,
    ReviewResultState,
};
pub use crate::wake::{
    classify_review_accounting_suffix_v1, classify_review_panel_invalid_v1,
    evaluate_review_panel_v1, review_deadline_secs, ReviewAccountingSuffixKindV1,
    ReviewPanelDecisionV1, ReviewPanelPolicyV1, ReviewPanelSeatStateV1, ReviewPanelSeatV1,
    REVIEW_ACCOUNTING_RECOVERY_CONTRACT_V2, REVIEW_PANEL_RUNTIME_CONTRACT_V1,
};

/// Replay whether one historical Panel can still reach its signed quorum.
pub(crate) fn review_panel_future_reachable_v1(
    events: &[orch_core::EventRecord],
    panel_id: &str,
    policy: &ReviewPoolPolicyV1,
) -> Result<bool> {
    crate::wake::review_panel_future_reachable_v1(events, panel_id, policy)
}

/// Validate the exact historical Panel exhaustion exception used by successor replay.
pub(crate) fn canonical_review_panel_exhaustion_terminal_for_successor(
    events: &[orch_core::EventRecord],
    terminal_position: usize,
    task: &str,
    attempt_id: &str,
    attempt_no: usize,
) -> Result<bool> {
    crate::wake::canonical_review_panel_exhaustion_terminal_for_successor(
        events,
        terminal_position,
        task,
        attempt_id,
        attempt_no,
    )
}

/// Decode one schema-1/2 task-card frontmatter snapshot without admitting a writer.
pub(crate) fn decode_card_meta_v1_v2(
    frontmatter: &str,
    relative_path: &str,
) -> Result<crate::card::CardMeta> {
    serde_yaml::from_str(frontmatter)
        .with_context(|| format!("legacy 任务卡 frontmatter 解析失败: {relative_path}"))
}

/// Decode one schema-1/2 ROUND-IR snapshot and bind its raw discriminator.
pub(crate) fn decode_round_ir_v1_v2(
    yaml: &str,
    schema_version: u32,
) -> std::result::Result<crate::plan::RoundIr, String> {
    let ir: crate::plan::RoundIr = serde_yaml::from_str(yaml)
        .map_err(|error| format!("解析 legacy signed ROUND-IR 失败: {error}"))?;
    if ir.schema_version != schema_version {
        return Err("ROUND-IR schemaVersion raw/typed 漂移".to_string());
    }
    Ok(ir)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One signed formal-review seat.
pub struct RequiredReview {
    /// Formal role occupied by this seat (`primary` or `secondary`).
    pub role: String,
    /// Default reviewer that must be tried for this role.
    pub agent: String,
}

/// One role-keyed fallback for a formal review seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewFallback {
    /// Formal role whose default reviewer must be exhausted first.
    pub role: String,
    /// Signed replacement reviewer admitted only after terminal, artifact-first
    /// validation of the default review channel.
    pub fallback_agent: String,
}

/// One signed nongate obligation and the invocation preset bound to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NongateSeat {
    /// Reviewer identity admitted for the nongate attempt.
    pub agent: String,
    /// Exact harness preset that must reach the signed invocation envelope.
    pub preset: String,
}

/// Closed policy governing substantive review voices for a new task card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewQuorumPolicy {
    /// Minimum number of unique substantive review results required for PASS.
    pub minimum_substantive: usize,
    /// Whether a signed nongate PASS may close a failed formal channel.
    pub nongate_may_substitute_failed_formal: bool,
    /// Minimum number of substantive nongate PASS results required before any
    /// formal-seat substitution is permitted.
    pub minimum_nongate_pass_for_substitution: usize,
}

/// Replay state of one runtime policy at an immutable attempt base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePolicyStateV1 {
    /// The policy exists in signed binding bytes but has no active authority.
    Dormant,
    /// A canonical activation or explicit carry-forward authorizes consumers.
    Active,
}

/// Explicit cross-round provenance required when a new binding starts active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicyCarryForwardV1 {
    /// Prior signed round that owns the source activation fact.
    pub source_round: String,
    /// Exact source activation event identity.
    pub source_event_id: String,
    /// SHA-256 of the prior policy subtree.
    pub source_policy_sha256: String,
    /// SHA-256 of the canonical source event JSON bytes.
    pub source_event_sha256: String,
}

/// One candidate in the signed review-pool union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewPoolCandidateV1 {
    /// Exact registered agent identity.
    pub agent: String,
    /// Maximum routed role for this candidate.
    pub role: String,
    /// Lineage used by the primary-PASS requirement.
    pub lineage: String,
    /// Whether a business-invalid generation can receive generation two.
    pub retry_eligible: bool,
    /// Optional candidate whose system failure this candidate backfills.
    #[serde(default)]
    pub fallback_for: Option<String>,
}

/// Exact signed descriptor for `review-pool-v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewPoolPolicyV1 {
    /// Descriptor schema version; only one is accepted.
    pub schema_version: u32,
    /// Recorded task that must own activation.
    pub owner_task: String,
    /// `dormant` in the defining round or `active` with carry-forward proof.
    pub initial_state: String,
    /// V1 policy scope; review-pool-v1 is round-scoped.
    pub scope: String,
    /// Minimum unique substantive PASS voices.
    pub minimum_passes: usize,
    /// Whether a counted PASS must have primary lineage.
    pub require_primary_pass: bool,
    /// Number of initial routes committed before any wake.
    pub initial_seat_count: usize,
    /// Minimum number of formal gate-eligible initial seats.
    pub minimum_gate_eligible: usize,
    /// Whole-panel business retry budget.
    pub maximum_business_retries: usize,
    /// Only formal role a nongate PASS may replace.
    pub nongate_substitutes_role: String,
    /// Ordered signed candidate union from which the planner selects.
    pub candidates: Vec<ReviewPoolCandidateV1>,
    /// Required provenance when a later round starts in active state.
    #[serde(default)]
    pub carry_forward: Option<RuntimePolicyCarryForwardV1>,
}

/// Fully verified policy state reconstructed from committed base bytes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePolicyResolutionV1 {
    /// Exact policy key requested by the caller.
    pub policy: String,
    /// Task that owns activation of this policy.
    pub owner_task: String,
    /// Dormant or active state at `policy_base_sha`.
    pub state: RuntimePolicyStateV1,
    /// SHA-256 of committed binding bytes signed by the committed ROUND-IR.
    pub binding_sha256: String,
    /// SHA-256 of the canonical selected policy subtree.
    pub policy_sha256: String,
    /// Activation/carry-forward event identity when active.
    pub activation_event_id: Option<String>,
    /// Immutable commit from which every field was resolved.
    pub policy_base_sha: String,
    /// Typed review-pool descriptor for `review-pool-v1`.
    pub review_pool: Option<ReviewPoolPolicyV1>,
}

/// Durable activation of one signed runtime policy at a captured main commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicyActivatedPayloadV1 {
    /// Payload schema discriminator; must equal [`RUNTIME_EVENT_SCHEMA_V1`].
    pub schema_version: u32,
    /// Exact key under `PROJECT-BINDING.runtimePolicies.policies`.
    pub policy: String,
    /// Task whose recorded implementation owns the policy.
    pub owner_task: String,
    /// Canonical TaskRecorded event authorizing activation.
    pub owner_recorded_event_id: String,
    /// Owner merge commit proven to be an ancestor of the activation base.
    pub owner_merge_sha: String,
    /// SHA-256 of the complete committed binding bytes.
    pub binding_sha256: String,
    /// SHA-256 of the canonical selected policy subtree.
    pub policy_sha256: String,
    /// Main commit captured before the accounting commit containing this fact.
    pub activated_at_main_sha: String,
}

/// Durable deactivation of one previously activated runtime policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicyDeactivatedPayloadV1 {
    /// Payload schema discriminator; must equal [`RUNTIME_EVENT_SCHEMA_V1`].
    pub schema_version: u32,
    /// Exact signed runtime-policy key.
    pub policy: String,
    /// Activation event whose state is being closed.
    pub activation_event_id: String,
    /// SHA-256 of the complete committed binding bytes.
    pub binding_sha256: String,
    /// SHA-256 of the canonical selected policy subtree.
    pub policy_sha256: String,
    /// Main commit captured before the deactivation accounting commit.
    pub deactivated_at_main_sha: String,
    /// Non-blank operator/runtime reason retained for audit.
    pub reason: String,
}

/// Atomic selection of one three-seat review panel for an immutable attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewPanelSelectedPayloadV1 {
    /// Payload schema discriminator.
    pub schema_version: u32,
    /// Stable panel identity shared by every route and terminal fact.
    pub panel_id: String,
    /// Canonical task attempt selected for review.
    pub attempt_id: String,
    /// Positive attempt ordinal matching `attemptId`.
    pub attempt_no: usize,
    /// Immutable candidate commit reviewed by every seat.
    pub reviewed_head: String,
    /// First DispatchIssued base commit that fixes policy semantics.
    pub policy_base_sha: String,
    /// Exact runtime-policy key that authorized dynamic review.
    pub policy: String,
    /// SHA-256 of that policy's canonical committed subtree.
    pub policy_sha256: String,
    /// V1 initial panel width; it must be exactly three.
    pub seat_count: usize,
    /// Exact three logical seat identities committed by the adjacent routes.
    pub seat_ids: [String; 3],
}

/// One precommitted route for a panel seat generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSeatRoutedPayloadV1 {
    /// Payload schema discriminator.
    pub schema_version: u32,
    /// Panel identity selected before any provider wake.
    pub panel_id: String,
    /// Stable logical seat identity retained across retry generations.
    pub seat_id: String,
    /// Positive, monotonic logical seat generation.
    pub generation: u32,
    /// Preallocated managed wake identity consumed by the spawn path.
    pub wake_id: String,
    /// Canonical task attempt receiving this route.
    pub attempt_id: String,
    /// Positive ordinal matching `attemptId`.
    pub attempt_no: usize,
    /// Routed role (`primary`, `secondary`, or `nongate`).
    pub role: String,
    /// Exact registered agent selected for this route.
    pub agent: String,
    /// Candidate lineage (`primary`, `secondary`, or `nongate`).
    pub lineage: String,
    /// Immutable candidate commit.
    pub reviewed_head: String,
    /// Attempt base commit fixing policy semantics.
    pub policy_base_sha: String,
    /// Deadline derived from role and signed workload.
    pub deadline_secs: u64,
    /// Whether a business-invalid result may receive generation two.
    pub retry_eligible: bool,
    /// Route source: `initial`, `retry`, or `backfill`.
    pub route_kind: String,
    /// ReviewPanelSelected event authorizing this route.
    pub selected_event_id: String,
    /// Source logical seat for retry/backfill; null only for initial routes.
    pub source_seat_id: Option<String>,
    /// Source logical generation for retry/backfill; null only for initial routes.
    pub source_generation: Option<u32>,
    /// Source terminal fact for retry/backfill; null only for initial routes.
    pub source_terminal_event_id: Option<String>,
}

/// Terminal classification of one exact panel seat generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSeatTerminatedPayloadV1 {
    /// Payload schema discriminator.
    pub schema_version: u32,
    /// Panel identity.
    pub panel_id: String,
    /// Stable logical seat identity.
    pub seat_id: String,
    /// Exact terminal generation.
    pub generation: u32,
    /// Exact managed wake identity.
    pub wake_id: String,
    /// Canonical task attempt.
    pub attempt_id: String,
    /// Positive ordinal matching `attemptId`.
    pub attempt_no: usize,
    /// Routed review role.
    pub role: String,
    /// Routed agent identity.
    pub agent: String,
    /// Candidate lineage copied from the route.
    pub lineage: String,
    /// Immutable reviewed candidate.
    pub reviewed_head: String,
    /// Attempt base commit fixing policy semantics.
    pub policy_base_sha: String,
    /// Closed state: pass/fail/blocked/business-invalid/system-terminal-invalid.
    pub state: String,
    /// Managed terminal or ActionRejected event proving the state.
    pub terminal_event_id: String,
    /// Delivery event for a substantive artifact; null for invalid terminals.
    pub delivery_event_id: Option<String>,
    /// Non-blank exact or derived terminal explanation.
    pub reason: String,
}

/// No-clobber promotion of one seat-scoped staging artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSpoolPromotedPayloadV1 {
    /// Payload schema discriminator.
    pub schema_version: u32,
    /// Panel identity owning the artifact.
    pub panel_id: String,
    /// Stable logical seat identity.
    pub seat_id: String,
    /// Exact seat generation.
    pub generation: u32,
    /// Exact managed wake identity.
    pub wake_id: String,
    /// Canonical task attempt.
    pub attempt_id: String,
    /// Positive ordinal matching `attemptId`.
    pub attempt_no: usize,
    /// Review role carried by the artifact.
    pub role: String,
    /// Reviewer identity carried by the artifact.
    pub agent: String,
    /// Immutable reviewed candidate.
    pub reviewed_head: String,
    /// Attempt base commit fixing policy semantics.
    pub policy_base_sha: String,
    /// Repository-relative lease-scoped staging path.
    pub staging_path: String,
    /// Repository-relative committed canonical path.
    pub canonical_path: String,
    /// SHA-256 of the promoted bytes.
    pub sha256: String,
    /// Complete artifact byte length.
    pub bytes: u64,
    /// Substantive body byte length.
    pub body_len: u64,
    /// Parsed artifact verdict.
    pub verdict: String,
    /// Managed terminal event that stabilized the staging bytes.
    pub terminal_event_id: String,
    /// Adjacent ReviewDelivered/NongateReviewDelivered event identity.
    pub delivery_event_id: String,
}

/// Durable close of one panel after pass, veto, or unreachable quorum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewPanelClosedPayloadV1 {
    /// Payload schema discriminator.
    pub schema_version: u32,
    /// Panel identity being closed.
    pub panel_id: String,
    /// Canonical task attempt.
    pub attempt_id: String,
    /// Positive ordinal matching `attemptId`.
    pub attempt_no: usize,
    /// Immutable reviewed candidate.
    pub reviewed_head: String,
    /// Attempt base commit fixing policy semantics.
    pub policy_base_sha: String,
    /// Close outcome: `pass`, `veto`, or `pool-exhausted`.
    pub outcome: String,
    /// Non-blank deterministic explanation.
    pub reason: String,
    /// Number of terminal seat generations observed at close.
    pub terminal_seat_count: usize,
    /// Number of unique PASS voices counted by policy.
    pub pass_count: usize,
    /// Whether at least one counted PASS has primary lineage.
    pub primary_pass: bool,
}

/// Runnable integration target reconstructed from an immutable schema-1/2 reader descriptor.
pub use archived_reader::{SourceReaderClosureDecisionV1, SourceReaderTargetV1};

fn require_legacy_reader_snapshots(
    root: &std::path::Path,
    round: &str,
    policy_base_sha: &str,
    candidate_sha: &str,
) -> Result<()> {
    for sha in [policy_base_sha, candidate_sha] {
        if sha.len() != 40
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            anyhow::bail!("legacy reader audit requires full immutable commit OIDs");
        }
        let bytes = crate::gitx::show_bytes(
            root,
            sha,
            &format!("coordination/rounds/{round}/events.jsonl"),
        )?;
        let events = std::str::from_utf8(&bytes)?
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<orch_core::EventRecord>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if crate::round::contract_schema_from_events(&events, round)?
            == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
        {
            anyhow::bail!("schema 3 must not enter the legacy reader audit");
        }
    }
    Ok(())
}

/// Independently replay historical source-reader selection from two exact Git commits.
/// This read-only boundary authenticates descriptors, ownership and unknown-reader escalation.
/// Schema3 snapshots and movable refs are rejected; it cannot inspect a mutable worktree,
/// run gates, append events or authorize a current write set.
///
/// Retired event producers are unavailable even through the host's public API:
/// ```compile_fail
/// use orch_host::verify::build_frozen_contract_superseded_events;
/// ```
/// ```compile_fail
/// let _ = orch_host::verify::FrozenContractSupersededPayload::from_signed_declaration;
/// ```
pub fn replay_source_reader_closure_v1(
    root: &std::path::Path,
    round: &str,
    candidate_sha: &str,
    policy_base_sha: &str,
    changed_paths: &[String],
) -> Result<SourceReaderClosureDecisionV1> {
    archived_reader::replay_at_tree(root, round, candidate_sha, policy_base_sha, changed_paths)
}

/// Reauthenticate the exact historical descriptor pair for an already-escalated receipt.
pub(crate) fn archived_source_reader_digests_v1(
    root: &std::path::Path,
    round: &str,
    policy_base_sha: &str,
    candidate_sha: &str,
) -> Result<(String, String)> {
    require_legacy_reader_snapshots(root, round, policy_base_sha, candidate_sha)?;
    archived_reader::authenticated_source_reader_digests_v1(root, policy_base_sha)
}

// The algorithm is required by independently checked r81/r82 receipt identities. It has no
// mutable-worktree resolver or live writer. Its only production callers are legacy replay.
mod archived_reader {
    use std::collections::{BTreeMap, BTreeSet};
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

    fn safe_target_component(value: &str) -> bool {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }

    /// Validate the independent integration target named by a production-source
    /// reader overlay.
    ///
    /// A reader under `crates/<package>/src` cannot derive a Cargo `--test` name
    /// from its own path.  Its descriptor must therefore bind a same-package,
    /// already-existing integration target at the immutable policy base.  Missing
    /// targets and package/test drift are errors so callers monotonically upgrade
    /// to the signed fast lane.
    fn production_reader_runnable_target_v1(
        root: &Path,
        policy_base_sha: &str,
        reader: &str,
        target: &RunnableTargetV1,
    ) -> Result<SourceReaderTargetV1> {
        if !canonical_repo_path(reader)
            || target.command_ref != SOURCE_READER_COMMAND
            || !safe_target_component(&target.package)
            || !safe_target_component(&target.test)
        {
            bail!("production reader runnableTarget 非 canonical: {reader}");
        }
        let components = Path::new(reader)
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => value.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>();
        let ["orch", "crates", package, "src", rest @ ..] = components.as_slice() else {
            bail!("production reader 不在 Cargo crate src 下: {reader}");
        };
        if rest.is_empty()
            || !rest.last().is_some_and(|file| file.ends_with(".rs"))
            || *package != target.package
        {
            bail!("production reader package/path 与 runnableTarget 漂移: {reader}");
        }
        let runnable_path = format!("orch/crates/{}/tests/{}.rs", target.package, target.test);
        if !gitx::tree_path_exists(root, policy_base_sha, &runnable_path)? {
            bail!("production reader runnable target 不存在: {runnable_path}");
        }
        let bytes = gitx::show_bytes(root, policy_base_sha, &runnable_path)
            .with_context(|| format!("读取 production reader target 失败: {runnable_path}"))?;
        if bytes.is_empty() || std::str::from_utf8(&bytes).is_err() {
            bail!("production reader runnable target 不是非空 UTF-8 Rust source");
        }
        Ok(SourceReaderTargetV1 {
            command_ref: target.command_ref.clone(),
            package: target.package.clone(),
            test: target.test.clone(),
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
            if depth == 0 && contains_rust_identifier(&bytes[expression_start..cursor - 1], b"test")
            {
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
        if (compact.contains("include_str!") || compact.contains("include_bytes!"))
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
                    if changed
                        .iter()
                        .any(|subject| same_crate_source(&reader, subject))
                    {
                        return Ok(Some((reader, literal)));
                    }
                    continue;
                };
                if changed.contains(&subject) {
                    return Ok(Some((reader, subject)));
                }
            }
            if dynamic
                && changed
                    .iter()
                    .any(|subject| same_crate_source(&reader, subject))
            {
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

    fn committed_round_event_paths(root: &Path, policy_base_sha: &str) -> Result<Vec<String>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "ls-tree",
                "-r",
                "--name-only",
                "-z",
                policy_base_sha,
                "--",
                "coordination/rounds",
            ])
            .output()
            .context("git ls-tree policy owner ledgers 启动失败")?;
        if !output.status.success() {
            bail!(
                "git ls-tree policy owner ledgers 失败({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut paths = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                std::str::from_utf8(entry)
                    .context("policy owner ledger path 非 UTF-8")
                    .map(str::to_string)
            })
            .collect::<Result<Vec<_>>>()?;
        paths.retain(|path| {
            let components = Path::new(path)
                .components()
                .filter_map(|component| match component {
                    Component::Normal(value) => value.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            matches!(
                components.as_slice(),
                ["coordination", "rounds", _, "events.jsonl"]
            )
        });
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Bind a runtime policy owner to a completed merge already contained in the
    /// immutable policy base.  A syntactically valid but unrecorded task id is not
    /// ownership authority.
    fn validate_recorded_policy_owner_v1(
        root: &Path,
        policy_base_sha: &str,
        owner_task: &str,
    ) -> Result<()> {
        crate::card::validate_task_id(owner_task).map_err(anyhow::Error::msg)?;
        let mut proofs = Vec::<String>::new();
        for path in committed_round_event_paths(root, policy_base_sha)? {
            let bytes = gitx::show_bytes(root, policy_base_sha, &path)
                .with_context(|| format!("读取 policy owner ledger 失败: {path}"))?;
            let mut events = Vec::<orch_core::EventRecord>::new();
            for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
                if line.is_empty() {
                    continue;
                }
                events.push(serde_json::from_slice(line).with_context(|| {
                    format!(
                        "policy owner ledger 非 canonical JSON: {path}:{}",
                        index + 1
                    )
                })?);
            }
            for (recorded_position, recorded) in events.iter().enumerate().filter(|(_, event)| {
                event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(owner_task)
            }) {
                let round = recorded
                    .round
                    .as_deref()
                    .context("policy owner TaskRecorded 缺 round")?;
                if recorded.actor != "runtime:orch"
                    || recorded
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("postMergeGates"))
                        .and_then(serde_json::Value::as_str)
                        != Some("all-green")
                {
                    bail!("policy owner TaskRecorded envelope/post-merge state 非 canonical");
                }
                let merges = events[..recorded_position]
                    .iter()
                    .filter(|event| {
                        event.kind == "MergeExecuted"
                            && event.actor == "reviewer:orch-runtime"
                            && event.round.as_deref() == Some(round)
                            && event.task_id.as_deref() == Some(owner_task)
                    })
                    .collect::<Vec<_>>();
                let [merged] = merges.as_slice() else {
                    bail!("policy owner TaskRecorded 前缺唯一 MergeExecuted");
                };
                let merge_sha = merged
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("mergeSha"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|sha| {
                        sha.len() == 40
                            && sha
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    })
                    .context("policy owner MergeExecuted.mergeSha 非 full commit")?;
                if !gitx::is_ancestor(root, merge_sha, policy_base_sha)? {
                    bail!("policy owner merge 不是 policy-base ancestor");
                }
                proofs.push(format!("{round}:{merge_sha}"));
            }
        }
        if proofs.len() != 1 {
            bail!(
                "candidate policy owner 必须恰有一个 recorded merge proof: owner={owner_task} found={}",
                proofs.len()
            );
        }
        Ok(())
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
            || crate::card::validate_task_id(&policy.owner_task).is_err()
            || !matches!(policy.initial_state.as_str(), "dormant" | "active")
            || policy.scope != "round"
            || policy.source_reader_closure.schema_version != 1
            || !valid_sha256(&policy.source_reader_closure.sha256)
            || !canonical_repo_path(&policy.source_reader_closure.path)
        {
            bail!("candidate-lanes-v1 policy/pointer contract 漂移");
        }
        validate_recorded_policy_owner_v1(root, policy_base_sha, &policy.owner_task)?;

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
    pub(super) fn authenticated_source_reader_digests_v1(
        root: &Path,
        policy_base_sha: &str,
    ) -> Result<(String, String)> {
        let (_, _, descriptor_sha256, base_sha256) = load_descriptor(root, policy_base_sha)?;
        Ok((descriptor_sha256, base_sha256))
    }

    fn build_reader_entries(
        root: &Path,
        policy_base_sha: &str,
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
            let derived =
                match integration_target(&overlay.reader, &overlay.runnable_target.command_ref) {
                    Ok(target) => target,
                    Err(_) => production_reader_runnable_target_v1(
                        root,
                        policy_base_sha,
                        &overlay.reader,
                        &overlay.runnable_target,
                    )?,
                };
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
            || manifest.edges.iter().any(|edge| {
                !canonical_repo_path(&edge.reader) || !canonical_repo_path(&edge.subject)
            })
            || manifest.edges.windows(2).any(|pair| pair[0] >= pair[1])
        {
            bail!("runtime escalation 清单 envelope/edge 非 canonical");
        }
        Ok(Some((manifest, digest)))
    }

    fn candidate_snapshot_bytes(
        root: &Path,
        candidate_sha: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>> {
        if !gitx::tree_path_exists(root, candidate_sha, path)? {
            return Ok(None);
        }
        gitx::show_bytes(root, candidate_sha, path)
            .map(Some)
            .with_context(|| format!("read archived candidate source failed: {path}"))
    }

    fn resolve_source_reader_closure_from_snapshot(
        root: &Path,
        snapshot: &str,
        round: Option<&str>,
        policy_base_sha: &str,
        changed_paths: &[String],
    ) -> Result<SourceReaderClosureDecisionV1> {
        let (descriptor, base, descriptor_sha, base_sha) =
            match load_descriptor(root, policy_base_sha) {
                Ok(value) => value,
                Err(error) => {
                    return Ok(SourceReaderClosureDecisionV1::UpgradeToFast {
                        reason: format!("source-reader descriptor/base drift: {error:#}"),
                        descriptor_sha256: String::new(),
                        base_sha256: String::new(),
                    });
                }
            };
        let entries = match build_reader_entries(root, policy_base_sha, &descriptor, &base) {
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
                        reason: format!(
                            "changed reader target disappeared or is not regular: {path}"
                        ),
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
                                    "registered production reader has no policy-base provenance: {path}: {error:#}"
                                ),
                                descriptor_sha256: descriptor_sha,
                                base_sha256: base_sha,
                            });
                        }
                    }
                    continue;
                }
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
                        reason: format!(
                            "selected reader lacks policy-base bytes: {reader}: {error:#}"
                        ),
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn historical_production_reader_retains_its_exact_runnable_target() {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
            let history = orch_core::read_ledger(&root.join("coordination/rounds/r82/events.jsonl")).unwrap();
            assert!(history.bad_lines.is_empty());
            let policy_base = history.events.iter().find_map(|event| {
                let payload = event.payload.as_ref()?;
                (event.kind == "DispatchIssued" && event.task_id.as_deref() == Some("B312")
                    && payload["attemptId"] == "B312-A0001")
                    .then(|| payload["baseSha"].as_str().unwrap().to_owned())
            }).expect("immutable post-B311 policy base");
            let (descriptor, base, _, _) = load_descriptor(root, &policy_base).unwrap();
            let entries = build_reader_entries(root, &policy_base, &descriptor, &base).unwrap();
            let target = &entries["orch/crates/orch-cli/src/guide.rs"].runnable;
            assert_eq!(target.reader, "orch/crates/orch-cli/src/guide.rs");
            assert_eq!(target.command_ref, "sourceReaderClosure");
            assert_eq!(target.package, "orch-cli");
            assert_eq!(target.test, "guide_cli");
        }

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

    /// Recompute a historical candidate closure from fixed commits only.
    pub(super) fn replay_at_tree(
        root: &Path,
        round: &str,
        candidate_sha: &str,
        policy_base_sha: &str,
        changed_paths: &[String],
    ) -> Result<SourceReaderClosureDecisionV1> {
        super::require_legacy_reader_snapshots(root, round, policy_base_sha, candidate_sha)?;
        resolve_source_reader_closure_from_snapshot(
            root,
            candidate_sha,
            Some(round),
            policy_base_sha,
            changed_paths,
        )
    }
}

// Read-only load projection and historical admission; no automatic action planner.
mod load_audit {
    use crate::generic_review::{generic_review_capacity_key, GenericReviewCapacityKey};
    use orch_core::EventRecord;
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::{Duration, SystemTime};
    /// Runtime work consumes the same per-agent capacity regardless of whether it
    /// is an implementation or a review.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum LoadKind {
        /// Implementation occupancy from recorded dispatch facts.
        Implementation,
        /// Review occupancy from exact request and termination facts.
        Review,
    }

    impl LoadKind {
        fn as_str(self) -> &'static str {
            match self {
                Self::Implementation => "impl",
                Self::Review => "review",
            }
        }
    }

    /// One durable capacity occupant projected from the round ledger.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LoadItem {
        /// Task recorded in the occupancy fact.
        pub task_id: String,
        /// Recorded implementation or review occupancy.
        pub kind: LoadKind,
        /// Exact attempt identity; other attempts cannot release it.
        pub attempt_id: String,
        /// Validated RFC3339 request or event timestamp.
        pub started_at: String,
        /// Recorded review role, absent for implementation occupancy.
        pub role: Option<String>,
    }

    /// Read-only admission values from a historical signed quota-domain projection.
    /// These values are not a schema-3 roster or a new scheduling policy.
    #[derive(Debug, Clone, Copy)]
    pub struct DomainAdmission<'a> {
        /// Alias being checked against the historical declaration.
        pub agent: &'a str,
        /// Historical quota-domain identity.
        pub domain: &'a str,
        /// Aliases declared in that historical domain.
        pub domain_members: &'a [String],
        /// Historical alias capacity.
        pub agent_capacity: usize,
        /// Historical domain capacity.
        pub domain_capacity: usize,
    }

    fn event_payload_string<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
        event
            .payload
            .as_ref()
            .and_then(|payload| payload.get(key))
            .and_then(serde_json::Value::as_str)
    }

    fn required_task_id(event: &EventRecord) -> Result<String, String> {
        event
            .task_id
            .as_ref()
            .filter(|task| !task.trim().is_empty())
            .cloned()
            .ok_or_else(|| format!("{} 缺 taskId，无法判定在飞负载", event.kind))
    }

    fn required_payload_string(event: &EventRecord, key: &str) -> Result<String, String> {
        event_payload_string(event, key)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("{} 缺 payload.{key}，无法判定在飞负载", event.kind))
    }

    fn load_item(
        event: &EventRecord,
        kind: LoadKind,
        role: Option<String>,
    ) -> Result<(String, LoadItem), String> {
        let task_id = required_task_id(event)?;
        let attempt_id = required_payload_string(event, "attemptId")?;
        let agent = required_payload_string(event, "agent")?;
        let started_at = event_payload_string(event, "requestedAt")
            .unwrap_or(&event.ts)
            .to_string();
        humantime::parse_rfc3339(&started_at)
            .map_err(|error| format!("{} 起算时刻 {started_at:?} 非法: {error}", event.kind))?;
        Ok((
            agent,
            LoadItem {
                task_id,
                kind,
                attempt_id,
                started_at,
                role,
            },
        ))
    }

    fn same_round(event: &EventRecord, round: &str) -> bool {
        event
            .round
            .as_deref()
            .is_none_or(|event_round| event_round == round)
    }

    /// Project all implementation and review capacity occupants from parsed
    /// events. Relevant malformed lifecycle facts fail closed instead of being
    /// silently treated as free capacity.
    pub fn agent_inflight_load_from_events(
        events: &[EventRecord],
        round: &str,
    ) -> Result<BTreeMap<String, Vec<LoadItem>>, String> {
        let mut implementations = BTreeMap::<(String, String), (String, LoadItem)>::new();
        let mut reviews = BTreeMap::<(String, String), (String, LoadItem)>::new();
        let mut generic_reviews = BTreeMap::<GenericReviewCapacityKey, (String, LoadItem)>::new();
        let mut generic_review_attempt_harnesses = BTreeSet::<(String, String, String)>::new();
        let mut generic_review_identities = BTreeMap::<String, GenericReviewCapacityKey>::new();
        let mut generic_review_closures = BTreeSet::<String>::new();
        let mut panel_reviews =
            BTreeMap::<(String, String, String, u32), (String, LoadItem)>::new();
        let mut panel_wakes = BTreeMap::<String, ((String, String, String, u32), String)>::new();
        let mut managed_panel_terminals = BTreeSet::<String>::new();

        for event in events.iter().filter(|event| same_round(event, round)) {
            match event.kind.as_str() {
                "DispatchIssued" => {
                    let (agent, item) = load_item(event, LoadKind::Implementation, None)?;
                    implementations.insert(
                        (item.task_id.clone(), item.attempt_id.clone()),
                        (agent, item),
                    );
                }
                "TaskRecorded" => {
                    let task_id = required_task_id(event)?;
                    implementations.retain(|(task, _), _| task != &task_id);
                    // A legacy formal channel can legitimately close through a
                    // signed nongate substitution and therefore never produce a
                    // ReviewDelivered for the failed reviewer; retaining that
                    // request after TaskRecorded would leak capacity forever.
                    // Panel routes are different: their managed process capacity
                    // is released only by the exact seat/wake terminal facts
                    // below, never by a task-level shortcut.
                    reviews.retain(|(task, _), _| task != &task_id);
                    let historical_closures = generic_reviews
                        .keys()
                        .filter(|(task, _, _, _)| task == &task_id)
                        .filter_map(|key| {
                            let closed = crate::generic_review::historical_b319_capacity_closed_v1(
                                events, round, &key.0, &key.1, &key.2, &key.3,
                            )
                            .and_then(|b319| {
                                if b319 {
                                    Ok(true)
                                } else {
                                    crate::generic_review::historical_b320_capacity_closed_v1(
                                        events, round, &key.0, &key.1, &key.2, &key.3,
                                    )
                                }
                            });
                            match closed {
                                Ok(true) => Some(Ok(key.clone())),
                                Ok(false) => None,
                                Err(error) => Some(Err(format!(
                                    "historical generic capacity closure invalid: {error:#}"
                                ))),
                            }
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    for key in historical_closures {
                        generic_reviews.remove(&key);
                    }
                }
                "AttemptBlocked" | "AttemptCrashed" | "AttemptTimedOut" | "AttemptFailed" => {
                    let task_id = required_task_id(event)?;
                    let attempt_id = required_payload_string(event, "attemptId")?;
                    implementations.remove(&(task_id.clone(), attempt_id.clone()));
                    let terminal_review_keys = reviews
                        .iter()
                        .filter(|((task, _), (_, pending))| {
                            task == &task_id && pending.attempt_id == attempt_id
                        })
                        .map(|(key, _)| key.clone())
                        .collect::<Vec<_>>();
                    for key in terminal_review_keys {
                        reviews.remove(&key);
                    }
                }
                // REPORT delivery begins remediation; it is deliberately not a
                // capacity release.
                "ReportObserved" => {}
                "ReviewRequested" => {
                    let role = required_payload_string(event, "role")?;
                    let wake_id = event_payload_string(event, "wakeId");
                    let panel_reserved = wake_id.is_some_and(|wake_id| {
                        panel_reviews.values().any(|(_, pending)| {
                            events.iter().any(|candidate| {
                                candidate.kind == "ReviewSeatRouted"
                                    && candidate.task_id.as_deref()
                                        == Some(pending.task_id.as_str())
                                    && event_payload_string(candidate, "attemptId")
                                        == Some(pending.attempt_id.as_str())
                                    && event_payload_string(candidate, "agent")
                                        == event_payload_string(event, "agent")
                                    && event_payload_string(candidate, "wakeId") == Some(wake_id)
                            })
                        })
                    });
                    if panel_reserved {
                        continue;
                    }
                    let (agent, item) = load_item(event, LoadKind::Review, Some(role.clone()))?;
                    if role == "review" {
                        let Some(key) = generic_review_capacity_key(event)? else {
                            return Err("generic ReviewRequested identity missing".to_string());
                        };
                        let attempt_harness = (key.0.clone(), key.1.clone(), key.2.clone());
                        if !generic_review_attempt_harnesses.insert(attempt_harness) {
                            return Err("duplicate generic review task/attempt/harness identity"
                                .to_string());
                        }
                        if generic_review_identities
                            .insert(key.3.clone(), key.clone())
                            .is_some()
                        {
                            return Err("duplicate generic review wakeId identity".to_string());
                        }
                        if generic_reviews.insert(key, (agent, item)).is_some() {
                            return Err("duplicate generic review capacity identity".to_string());
                        }
                        continue;
                    }
                    // B157 defines one current expectation per task/role. A new
                    // request supersedes that exact slot, including its old agent.
                    reviews.insert((item.task_id.clone(), role), (agent, item));
                }
                "ReviewDelivered" => {
                    let task_id = required_task_id(event)?;
                    let attempt_id = required_payload_string(event, "attemptId")?;
                    let role = required_payload_string(event, "role")?;
                    let agent = required_payload_string(event, "agent")?;
                    let substantive = event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("bodyLen"))
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|length| length > 0);
                    if role == "review" {
                        let harness = required_payload_string(event, "harness")?;
                        let wake_id = required_payload_string(event, "wakeId")?;
                        if harness != agent {
                            return Err("generic ReviewDelivered agent/harness drift".to_string());
                        }
                        let key = (task_id, attempt_id, harness, wake_id.clone());
                        if generic_review_identities.get(&wake_id) != Some(&key) {
                            return Err("generic ReviewDelivered identity is not an exact request"
                                .to_string());
                        }
                        continue;
                    }
                    if substantive
                        && reviews.get(&(task_id.clone(), role.clone())).is_some_and(
                            |(pending_agent, pending)| {
                                pending_agent == &agent && pending.attempt_id == attempt_id
                            },
                        )
                    {
                        reviews.remove(&(task_id, role));
                    }
                }
                "ReviewSeatRouted" => {
                    let decoded = crate::ledger::decode_runtime_event_v1(event)
                        .map_err(|error| format!("ReviewSeatRouted decode failed: {error:#}"))?;
                    let Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatRouted(route)) =
                        decoded
                    else {
                        return Err("ReviewSeatRouted kind/payload mismatch".to_string());
                    };
                    let wake_id = route.wake_id.clone();
                    let (agent, item) = load_item(event, LoadKind::Review, Some(route.role))?;
                    let key = (
                        item.task_id.clone(),
                        item.attempt_id.clone(),
                        route.seat_id,
                        route.generation,
                    );
                    if panel_reviews
                        .insert(key.clone(), (agent.clone(), item))
                        .is_some()
                        || panel_wakes.insert(wake_id, (key, agent)).is_some()
                    {
                        return Err("duplicate panel route capacity identity".to_string());
                    }
                }
                "ReviewSeatTerminated" => {
                    let decoded =
                        crate::ledger::decode_runtime_event_v1(event).map_err(|error| {
                            format!("ReviewSeatTerminated decode failed: {error:#}")
                        })?;
                    let Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatTerminated(terminal)) =
                        decoded
                    else {
                        return Err("ReviewSeatTerminated kind/payload mismatch".to_string());
                    };
                    let task_id = required_task_id(event)?;
                    let key = (
                        task_id,
                        terminal.attempt_id,
                        terminal.seat_id,
                        terminal.generation,
                    );
                    let Some((routed_key, routed_agent)) = panel_wakes.get(&terminal.wake_id)
                    else {
                        return Err(
                            "ReviewSeatTerminated lacks routed capacity identity".to_string()
                        );
                    };
                    if event.actor != "runtime:orch"
                        || event.round.as_deref() != Some(round)
                        || routed_key != &key
                        || routed_agent != &terminal.agent
                    {
                        return Err(
                            "ReviewSeatTerminated capacity authority is not exact".to_string()
                        );
                    }
                    panel_reviews.remove(&key);
                }
                "ManagedWakeTerminated" => {
                    let Some(wake_id) = event_payload_string(event, "wakeId") else {
                        continue;
                    };
                    if let Some(key) = generic_review_identities.get(wake_id) {
                        let exact_identity = event.actor == "runtime:orch"
                            && event.round.as_deref() == Some(round)
                            && event.task_id.as_deref() == Some(key.0.as_str())
                            && event_payload_string(event, "agent") == Some(key.2.as_str());
                        if !exact_identity {
                            return Err(
                                "generic ManagedWakeTerminated authority is not exact".to_string()
                            );
                        }
                        let wakes = events
                            .iter()
                            .filter(|candidate| {
                                candidate.kind == "WakeIssued"
                                    && event_payload_string(candidate, "wakeId") == Some(wake_id)
                            })
                            .collect::<Vec<_>>();
                        let [wake] = wakes.as_slice() else {
                            return Err(
                                "generic ManagedWakeTerminated 缺唯一 WakeIssued".to_string()
                            );
                        };
                        if generic_review_capacity_key(wake)?.as_ref() != Some(key)
                            || crate::wake::validate_existing_channel_action_binding(wake, event)
                                .is_err()
                        {
                            return Err(
                                "generic ManagedWakeTerminated identity/binding 非可信".to_string()
                            );
                        }
                        if !crate::generic_review::generic_review_terminal_closed_v1(event) {
                            // An exact but internally inconsistent historical terminal
                            // never releases capacity. Keep the occupant until an exact
                            // attempt terminal or another canonical closure; root review
                            // validation will reject this fact rather than trusting it.
                            continue;
                        }
                        if !generic_review_closures.insert(wake_id.to_string()) {
                            return Err("generic ManagedWakeTerminated 重复".to_string());
                        }
                        continue;
                    }
                    let Some((key, routed_agent)) = panel_wakes.get(wake_id) else {
                        continue;
                    };
                    let exact = event.actor == "runtime:orch"
                        && event.round.as_deref() == Some(round)
                        && event.task_id.as_deref() == Some(key.0.as_str())
                        && event_payload_string(event, "agent") == Some(routed_agent.as_str())
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("managedScopeTerminated"))
                            .and_then(serde_json::Value::as_bool)
                            == Some(true);
                    if !exact || !managed_panel_terminals.insert(wake_id.to_string()) {
                        return Err(
                            "panel ManagedWakeTerminated authority is not exact".to_string()
                        );
                    }
                    panel_reviews.remove(key);
                }
                "WorkspaceReleased" => {
                    let Some(wake_id) = event_payload_string(event, "wakeId") else {
                        continue;
                    };
                    let Some(key) = generic_review_identities.get(wake_id) else {
                        continue;
                    };
                    if !generic_review_closures.contains(wake_id) {
                        // A release cannot upgrade an absent/untrusted terminal.
                        // Keep the capacity occupant for an exact later attempt
                        // terminal; root validation still rejects the bad pair.
                        continue;
                    }
                    crate::generic_review::generic_review_terminal_release_closed_v1(
                        events, round, &key.0, &key.1, &key.2, &key.3,
                    )
                    .map_err(|error| format!("generic WorkspaceReleased 非 exact: {error:#}"))?;
                    generic_reviews.remove(key);
                }
                "ActionRejected" => {
                    let Some(wake_id) = event_payload_string(event, "actionId") else {
                        continue;
                    };
                    let Some(key) = generic_review_identities.get(wake_id) else {
                        continue;
                    };
                    let payload = event
                        .payload
                        .as_ref()
                        .and_then(serde_json::Value::as_object);
                    let exact = event.actor == "runtime:orch"
                        && event.round.as_deref() == Some(round)
                        && event.task_id.as_deref() == Some(key.0.as_str())
                        && event_payload_string(event, "attemptId") == Some(key.1.as_str())
                        && payload.is_some_and(|payload| payload.len() == 7)
                        && matches!(event_payload_string(event, "operation"), Some("wake-backend-receipt" | "review-quarantine"))
                        && event_payload_string(event, "reason")
                            .is_some_and(|reason| !reason.trim().is_empty())
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("exitCode"))
                            .and_then(serde_json::Value::as_i64)
                            .is_some_and(|value| (1..=255).contains(&value))
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("attemptNo"))
                            .and_then(serde_json::Value::as_u64)
                            .is_some_and(|value| value > 0)
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("alert"))
                            .and_then(serde_json::Value::as_bool)
                            == Some(true);
                    if !exact {
                        return Err("generic ActionRejected authority is not exact".to_string());
                    }
                    if crate::generic_review::is_review_quarantine(event) {
                        crate::generic_review::validate_review_quarantine_shape(event)
                            .map_err(|error| error.to_string())?;
                    }
                    // A degraded post-spawn rejection closes adjudication only;
                    // it cannot release managed process/workspace capacity.
                }
                _ => {}
            }
        }

        let mut load = BTreeMap::<String, Vec<LoadItem>>::new();
        for (agent, item) in implementations
            .into_values()
            .chain(reviews.into_values())
            .chain(generic_reviews.into_values())
            .chain(panel_reviews.into_values())
        {
            load.entry(agent).or_default().push(item);
        }
        for items in load.values_mut() {
            items.sort_by(|left, right| {
                (&left.started_at, &left.task_id, left.kind, &left.attempt_id).cmp(&(
                    &right.started_at,
                    &right.task_id,
                    right.kind,
                    &right.attempt_id,
                ))
            });
        }
        Ok(load)
    }

    /// Strict JSONL entry used by contract tests and callers that only have raw
    /// ledger bytes. One malformed non-empty line rejects the whole projection.
    pub fn agent_inflight_load(
        ledger_jsonl: &str,
        round: &str,
    ) -> Result<BTreeMap<String, Vec<LoadItem>>, String> {
        let mut events = Vec::new();
        for (index, line) in ledger_jsonl.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            events.push(
                serde_json::from_str::<EventRecord>(line)
                    .map_err(|error| format!("ledger 第 {} 行无法解析: {error}", index + 1))?,
            );
        }
        agent_inflight_load_from_events(&events, round)
    }

    fn load_details(items: &[LoadItem], now: SystemTime) -> Result<String, String> {
        items
            .iter()
            .map(|item| {
                let started = humantime::parse_rfc3339(&item.started_at)
                    .map_err(|error| format!("负载 {} 起算时刻非法: {error}", item.attempt_id))?;
                let age = now.duration_since(started).unwrap_or(Duration::ZERO);
                let role = item
                    .role
                    .as_deref()
                    .map(|role| format!("/{role}"))
                    .unwrap_or_default();
                Ok(format!(
                    "{}:{}{} attempt={} age={}s started={}",
                    item.task_id,
                    item.kind.as_str(),
                    role,
                    item.attempt_id,
                    age.as_secs(),
                    item.started_at
                ))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(|details| details.join(", "))
    }

    /// Apply the existing counted agent/quota scheduler semantics to a real
    /// runtime admission. Both lanes observe the same durable work set because the
    /// signed IR stores both limits per agent.
    pub fn capacity_admits_in_domain(
        load: &BTreeMap<String, usize>,
        request: &DomainAdmission<'_>,
    ) -> Result<(), String> {
        if request.agent.trim().is_empty() {
            return Err("runtime admission agent 为空".into());
        }
        if request.domain.trim().is_empty() {
            return Err(format!(
                "agent {} 的 runtime admission quotaDomain 为空",
                request.agent
            ));
        }

        let members = request
            .domain_members
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if !members.contains(request.agent) {
            return Err(format!(
                "quotaDomain {} 成员表不含请求 agent {}",
                request.domain, request.agent
            ));
        }
        let agent_load = load.get(request.agent).copied().unwrap_or(0);
        let domain_load = members
            .iter()
            .map(|member| load.get(*member).copied().unwrap_or(0))
            .sum::<usize>();

        // Stable precedence for an input that violates both layers: the narrower
        // per-agent qualification is reported first. Both errors name their layer
        // and include the other lane's measurement for operator diagnosis.
        if agent_load >= request.agent_capacity {
            return Err(format!(
            "agent {} 容量已满：layer=agent load={} agentCapacity={} quotaDomain={} domainLoad={} domainCapacity={}",
            request.agent,
            agent_load,
            request.agent_capacity,
            request.domain,
            domain_load,
            request.domain_capacity
        ));
        }
        if domain_load >= request.domain_capacity {
            return Err(format!(
            "quotaDomain {} 容量已满：layer=domain domainLoad={} domainCapacity={} requestedAgent={} agentLoad={} agentCapacity={}",
            request.domain,
            domain_load,
            request.domain_capacity,
            request.agent,
            agent_load,
            request.agent_capacity
        ));
        }
        Ok(())
    }

    fn capacity_admits_in_domain_with_items(
        load: &BTreeMap<String, Vec<LoadItem>>,
        request: &DomainAdmission<'_>,
    ) -> Result<(), String> {
        let counts = load
            .iter()
            .map(|(agent, items)| (agent.clone(), items.len()))
            .collect::<BTreeMap<_, _>>();
        let error = match capacity_admits_in_domain(&counts, request) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        let agent_load = counts.get(request.agent).copied().unwrap_or(0);
        let mut occupying = Vec::new();
        if agent_load >= request.agent_capacity {
            if let Some(items) = load.get(request.agent) {
                occupying.extend(items.iter().cloned());
            }
        } else {
            let members = request
                .domain_members
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            for member in members {
                if let Some(items) = load.get(member) {
                    occupying.extend(items.iter().cloned());
                }
            }
        }
        let details = load_details(&occupying, SystemTime::now())?;
        Err(format!("{error}；占用者=[{details}]"))
    }

    /// Compatibility surface for callers that still provide the historical
    /// per-agent `agent/quota` pair. It is represented as a singleton domain, so
    /// admission results remain byte-for-byte equivalent in the single-instance
    /// case while sharing the new implementation.
    pub fn capacity_admits_with_limits(
        load: &BTreeMap<String, Vec<LoadItem>>,
        agent: &str,
        agent_capacity: usize,
        quota_capacity: usize,
    ) -> Result<(), String> {
        let members = [agent.to_string()];
        capacity_admits_in_domain_with_items(
            load,
            &DomainAdmission {
                agent,
                domain: agent,
                domain_members: &members,
                agent_capacity,
                domain_capacity: quota_capacity,
            },
        )
    }

    /// Single-limit compatibility surface used by the seeded contract. Runtime
    /// callers use [`capacity_admits_with_limits`] so both signed lanes are
    /// enforced.
    pub fn capacity_admits(
        load: &BTreeMap<String, Vec<LoadItem>>,
        agent: &str,
        capacity: usize,
    ) -> Result<(), String> {
        capacity_admits_with_limits(load, agent, capacity, capacity)
    }

    /// Runtime role checks reuse the plan-time subset predicate verbatim.
    pub fn role_admits(roles: &[String], required: &str) -> Result<(), String> {
        crate::plan::capability_supported(&[required.to_string()], roles)
            .map_err(|error| format!("runtime role 拒绝 {required}: {error}"))
    }

    fn scheduling_admits_projected_load(
        load: &BTreeMap<String, Vec<LoadItem>>,
        scheduling: &crate::plan::IrScheduling,
        agent: &str,
    ) -> Result<(), String> {
        let domain = scheduling.quota_domain_for(agent)?;
        let domain_members = scheduling.quota_domain_members(agent)?;
        capacity_admits_in_domain_with_items(
            load,
            &DomainAdmission {
                agent,
                domain,
                domain_members: &domain_members,
                agent_capacity: scheduling.effective_agent_capacity(agent)?,
                domain_capacity: scheduling.effective_domain_capacity_for_agent(agent)?,
            },
        )
    }

    fn scheduling_identity_admits(
        scheduling: &crate::plan::IrScheduling,
        agent: &str,
        required_role: &str,
    ) -> Result<(), String> {
        if !scheduling
            .allowed_agents
            .iter()
            .any(|allowed| allowed == agent)
        {
            return Err(format!("agent {agent} 未获 active ROUND-IR 授权"));
        }
        let capacity = scheduling
            .capacities
            .get(agent)
            .ok_or_else(|| format!("agent {agent} 缺 active ROUND-IR capacity"))?;
        role_admits(&capacity.roles, required_role)
    }

    /// Shared runtime gate consumed by both implementation dispatch and review
    /// injection.
    pub fn scheduling_admits(
        events: &[EventRecord],
        round: &str,
        scheduling: &crate::plan::IrScheduling,
        agent: &str,
        required_role: &str,
    ) -> Result<(), String> {
        scheduling_identity_admits(scheduling, agent, required_role)?;
        let load = agent_inflight_load_from_events(events, round)?;
        scheduling_admits_projected_load(&load, scheduling, agent)
    }

    /// Admit a formal-review reissue after subtracting exactly the durable slot it
    /// replaces.  This is not a capacity bypass: identity/role checks and both the
    /// per-agent and shared quota-domain limits run unchanged against the remaining
    /// projection.  The caller must first authenticate the source wake/death tuple;
    /// this function additionally refuses unless that exact tuple occupies one and
    /// only one current review slot.
    pub(crate) fn scheduling_admits_replacing_review_slot(
        events: &[EventRecord],
        round: &str,
        scheduling: &crate::plan::IrScheduling,
        agent: &str,
        required_role: &str,
        task_id: &str,
        attempt_id: &str,
        review_role: &str,
    ) -> Result<(), String> {
        let role_matches = matches!(
            (review_role, required_role),
            ("primary", "primary-review") | ("secondary", "secondary-review")
        );
        if !role_matches {
            return Err(format!(
            "review reissue replacement role mismatch: slot={review_role} required={required_role}"
        ));
        }
        scheduling_identity_admits(scheduling, agent, required_role)?;
        let mut load = agent_inflight_load_from_events(events, round)?;
        let positions = load
            .get(agent)
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, item)| {
                (item.kind == LoadKind::Review
                    && item.task_id == task_id
                    && item.attempt_id == attempt_id
                    && item.role.as_deref() == Some(review_role))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if positions.len() != 1 {
            return Err(format!(
            "review reissue replacement requires exactly one current slot: agent={agent} task={task_id} attempt={attempt_id} role={review_role} found={}",
            positions.len()
        ));
        }
        let remove_agent = {
            let items = load
                .get_mut(agent)
                .expect("one matching replacement position implies an agent load entry");
            items.remove(positions[0]);
            items.is_empty()
        };
        if remove_agent {
            load.remove(agent);
        }
        scheduling_admits_projected_load(&load, scheduling, agent)
    }
}
/// Read-only occupancy facts and historical admission checks used by retained callers.
pub use load_audit::{
    agent_inflight_load, agent_inflight_load_from_events, capacity_admits,
    capacity_admits_in_domain, capacity_admits_with_limits, role_admits, scheduling_admits,
    DomainAdmission, LoadItem, LoadKind,
};

pub(crate) use load_audit::scheduling_admits_replacing_review_slot;

#[cfg(test)]
mod load_audit_tests {
    use super::*;
    use crate::generic_review::validate_generic_review_admission_v1;
    use orch_core::EventRecord;

    fn b246_review_scheduling() -> crate::plan::IrScheduling {
        serde_yaml::from_str(
            r#"
allowedAgents: [executor-opencode, executor-opencode-scout]
capacities:
  executor-opencode: {agent: 1, quota: 1, roles: [primary-review, secondary-review]}
  executor-opencode-scout: {agent: 1, quota: 1, roles: [primary-review, secondary-review]}
agentDomains:
  executor-opencode: opencode
  executor-opencode-scout: opencode
quotaDomains:
  opencode: {agent: 1, quota: 1}
"#,
        )
        .expect("B246 shared-domain scheduling fixture must parse")
    }

    fn b246_review_request(
        task_id: &str,
        attempt_id: &str,
        role: &str,
        agent: &str,
    ) -> EventRecord {
        crate::ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some(task_id),
            Some("rB246"),
            serde_json::json!({
                "attemptId": attempt_id,
                "role": role,
                "agent": agent,
                "requestedAt": "2026-08-09T00:00:00Z",
            }),
        )
    }

    fn b262_attempt_terminal(task_id: &str, attempt_id: &str) -> EventRecord {
        crate::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(task_id),
            Some("rB246"),
            serde_json::json!({
                "attemptId": attempt_id,
                "agent": "executor-desktop",
            }),
        )
    }

    fn b262_review_delivery(
        task_id: &str,
        attempt_id: &str,
        role: &str,
        agent: &str,
        body_len: u64,
    ) -> EventRecord {
        crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some(task_id),
            Some("rB246"),
            serde_json::json!({
                "attemptId": attempt_id,
                "role": role,
                "agent": agent,
                "bodyLen": body_len,
            }),
        )
    }

    fn b262_review_items(events: &[EventRecord]) -> Vec<LoadItem> {
        agent_inflight_load_from_events(events, "rB246")
            .unwrap()
            .into_values()
            .flatten()
            .filter(|item| item.kind == LoadKind::Review)
            .collect()
    }

    fn b321_generic_request(harness: &str, wake_id: &str) -> EventRecord {
        crate::ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "attemptId": "B321-A0001",
                "role": "review",
                "agent": harness,
                "harness": harness,
                "wakeId": wake_id,
                "requestedAt": "2026-09-02T00:00:00Z",
            }),
        )
    }

    fn b321_generic_binding(harness: &str) -> serde_json::Value {
        serde_json::json!({
            "configDigest": "1".repeat(64),
            "requestDigest": "2".repeat(64),
            "attachmentManifestSha256": "3".repeat(64),
            "commandDigest": "4".repeat(64),
            "executableIdentityDigest": "5".repeat(64),
            "requestedTuple": {"provider": null, "model": null, "effort": null, "mode": null},
            "effectiveTuple": {"provider": null, "model": null, "effort": null, "mode": null},
            "driver": "claude", "harness": harness,
            "observationSource": "claude-stream-json",
            "invocationCwd": "/repo/.worktrees/review-alpha",
            "cwdSelection": "target-worktree",
            "fixedHead": "0123456789012345678901234567890123456789"
        })
    }

    fn b321_generic_terminal(harness: &str, wake_id: &str, state: &str) -> EventRecord {
        let (
            completion_reason,
            terminal_seen,
            turn_ended,
            exited_naturally,
            hard_deadline_reached,
            cancel_request_id,
            signals,
            outcome_class,
        ) = match state {
            "answered" | "empty" => (
                "natural-exit",
                true,
                true,
                true,
                false,
                serde_json::Value::Null,
                serde_json::json!([]),
                "DeliveredTerminal",
            ),
            "timedOut" => (
                "hard-deadline",
                false,
                false,
                false,
                true,
                serde_json::Value::Null,
                serde_json::json!(["TERM"]),
                "StoppedByHardDeadline",
            ),
            "canceled" => (
                "manual-cancel",
                false,
                false,
                false,
                false,
                serde_json::json!("cancel-1"),
                serde_json::json!(["TERM"]),
                "StoppedByAuthenticatedCancel",
            ),
            "failed" => (
                "operational-error",
                false,
                false,
                false,
                false,
                serde_json::Value::Null,
                serde_json::json!([]),
                "OperationalError",
            ),
            other => panic!("unknown fixture terminal state {other}"),
        };
        let mut payload = serde_json::json!({
            "agent": harness,
            "wakeId": wake_id,
            "state": state,
            "completionReason": completion_reason,
            "exactReason": "fixture-terminal",
            "terminalSeen": terminal_seen,
            "turnEnded": turn_ended,
            "exitedNaturally": exited_naturally,
            "hardDeadlineReached": hard_deadline_reached,
            "cancelRequestId": cancel_request_id,
            "signals": signals,
            "managedScopeTerminated": true,
            "logBytesRead": 1,
            "outcomeClass": outcome_class,
            "channelBinding": b321_generic_binding(harness),
        });
        if state == "answered" {
            payload["outputPath"] = serde_json::json!("/repo/review.md");
            payload["outputSha256"] = serde_json::json!("a".repeat(64));
        }
        crate::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            payload,
        )
    }

    fn b321_generic_closed_events(harness: &str, wake_id: &str, state: &str) -> Vec<EventRecord> {
        let lease = crate::ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "siteId": format!("B321-review-{harness}-g01"), "generation": 1,
                "attemptId": "B321-A0001", "role": "review", "agent": harness,
                "wakeId": wake_id,
                "paths": {"worktree": ".worktrees/review-alpha", "target": "orch/target/review-alpha"}
            }),
        );
        let mut wake_payload = b321_generic_binding(harness);
        wake_payload.as_object_mut().unwrap().extend(
            serde_json::json!({
                "method": "unified-channel-v1", "action": "review",
                "attemptId": "B321-A0001", "agent": harness, "harness": harness,
                "wakeId": wake_id,
                "continuationId": format!("review:rB321:B321:B321-A0001:review:{harness}")
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let wake = crate::ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            wake_payload,
        );
        let request = b321_generic_request(harness, wake_id);
        let terminal = b321_generic_terminal(harness, wake_id, state);
        let release = crate::ledger::event(
            "WorkspaceReleased",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "siteId": format!("B321-review-{harness}-g01"), "generation": 1,
                "attemptId": "B321-A0001", "role": "review", "agent": harness,
                "wakeId": wake_id,
                "completionReceipt": "runtime:orch/managed-wake-terminated",
                "terminationEventId": terminal.event_id.clone()
            }),
        );
        vec![lease, wake, request, terminal, release]
    }

    fn b321_generic_rejection(wake_id: &str) -> EventRecord {
        crate::ledger::event(
            "ActionRejected",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "actionId": wake_id,
                "attemptId": "B321-A0001",
                "attemptNo": 1,
                "operation": "wake-backend-receipt",
                "reason": "typed fixture rejection",
                "exitCode": 2,
                "alert": true,
            }),
        )
    }

    fn b321_generic_review_count(events: &[EventRecord]) -> usize {
        agent_inflight_load_from_events(events, "rB321")
            .expect("generic review capacity projection")
            .into_values()
            .flatten()
            .filter(|item| item.kind == LoadKind::Review)
            .count()
    }

    #[test]
    fn b321_generic_admission_rejects_attempt_harness_or_wake_reuse() {
        let first = b321_generic_request("alpha", "wake-alpha");
        validate_generic_review_admission_v1(
            &[],
            "rB321",
            "B321",
            "B321-A0001",
            "alpha",
            "wake-alpha",
        )
        .expect("fresh exact identity is admitted");
        assert!(validate_generic_review_admission_v1(
            std::slice::from_ref(&first),
            "rB321",
            "B321",
            "B321-A0001",
            "alpha",
            "wake-new",
        )
        .is_err());
        assert!(validate_generic_review_admission_v1(
            std::slice::from_ref(&first),
            "rB321",
            "B321",
            "B321-A0001",
            "beta",
            "wake-alpha",
        )
        .is_err());
        validate_generic_review_admission_v1(
            std::slice::from_ref(&first),
            "rB321",
            "B321",
            "B321-A0001",
            "beta",
            "wake-beta",
        )
        .expect("different harness and wake remain independent");

        let lease = crate::ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "attemptId": "B321-A0001",
                "role": "review",
                "agent": "gamma",
                "wakeId": "wake-gamma",
            }),
        );
        assert!(validate_generic_review_admission_v1(
            std::slice::from_ref(&lease),
            "rB321",
            "B321",
            "B321-A0001",
            "gamma",
            "wake-other",
        )
        .is_err());

        let rejection = crate::ledger::event(
            "ActionRejected",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "actionId": "wake-gamma", "operation": "wake",
                "reason": "pre-spawn fixture rejection", "exitCode": 2,
                "alert": true, "attemptId": "B321-A0001", "attemptNo": 1
            }),
        );
        let mut retired_lease = lease.clone();
        retired_lease.payload = Some(serde_json::json!({
            "siteId": "B321-review-gamma-g01", "generation": 1,
            "attemptId": "B321-A0001", "role": "review", "agent": "gamma",
            "reviewedHead": "0123456789012345678901234567890123456789",
            "wakeId": "wake-gamma",
            "paths": {"worktree": ".worktrees/review-gamma", "target": "orch/target/review-gamma"}
        }));
        let retirement = crate::ledger::event(
            crate::sites::SITE_RETIRED_EVENT_KIND,
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "siteId": "B321-review-gamma-g01", "generation": 1,
                "taskId": "B321", "attemptId": "B321-A0001", "role": "review",
                "agent": "gamma", "wakeId": "wake-gamma",
                "trigger": "pre-spawn-rejected", "retireEventId": rejection.event_id.clone()
            }),
        );
        validate_generic_review_admission_v1(
            &[retired_lease, rejection, retirement],
            "rB321",
            "B321",
            "B321-A0001",
            "gamma",
            "wake-retry",
        )
        .expect("a proven no-spawn retirement does not burn the harness identity");
    }

    #[test]
    fn b321_generic_projection_keys_all_four_fields_and_closes_on_exact_terminal() {
        let request = b321_generic_request("alpha", "wake-alpha");
        assert_eq!(b321_generic_review_count(std::slice::from_ref(&request)), 1);
        for state in ["answered", "empty", "timedOut", "canceled", "failed"] {
            assert_eq!(
                b321_generic_review_count(&b321_generic_closed_events(
                    "alpha",
                    "wake-alpha",
                    state,
                )),
                0,
                "trusted exact {state} terminal+release closes without ReviewDelivered"
            );
        }
        let mut reordered = b321_generic_closed_events("alpha", "wake-alpha", "empty");
        reordered.swap(0, 1);
        assert!(agent_inflight_load_from_events(&reordered, "rB321").is_err());
        assert_eq!(
            b321_generic_review_count(&[request.clone(), b321_generic_rejection("wake-alpha"),]),
            1,
            "post-spawn ActionRejected cannot release physical capacity"
        );
        let mut rejected_then_closed = b321_generic_closed_events("alpha", "wake-alpha", "failed");
        rejected_then_closed.insert(3, b321_generic_rejection("wake-alpha"));
        assert_eq!(b321_generic_review_count(&rejected_then_closed), 0);

        let mut untrusted = b321_generic_closed_events("alpha", "wake-alpha", "empty");
        untrusted[3].payload.as_mut().unwrap()["outcomeClass"] =
            serde_json::json!("TruncatedNoTerminal");
        assert_eq!(
            b321_generic_review_count(&untrusted),
            1,
            "an exact but inconsistent terminal/release cannot free capacity"
        );
        let mut root_blocked = untrusted.clone();
        let root_event = crate::ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "attemptId": "B321-A0001", "verdict": "BLOCKED",
                "headSha": "0123456789012345678901234567890123456789"
            }),
        );
        root_blocked.push(root_event.clone());
        assert_eq!(b321_generic_review_count(&root_blocked), 1);
        let mut wrong_head = untrusted.clone();
        let mut wrong_root = root_event.clone();
        wrong_root.payload.as_mut().unwrap()["headSha"] = serde_json::json!("a".repeat(40));
        wrong_head.push(wrong_root);
        assert_eq!(b321_generic_review_count(&wrong_head), 1);
        let mut duplicate_root = untrusted.clone();
        duplicate_root.extend([root_event.clone(), root_event]);
        assert_eq!(b321_generic_review_count(&duplicate_root), 1);
        untrusted.push(crate::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({"attemptId": "B321-A0001"}),
        ));
        assert_eq!(b321_generic_review_count(&untrusted), 1);
        assert!(agent_inflight_load_from_events(
            &[
                request.clone(),
                b321_generic_terminal("beta", "wake-alpha", "empty"),
            ],
            "rB321",
        )
        .is_err());

        let duplicate_harness = [
            request.clone(),
            b321_generic_request("alpha", "wake-second"),
        ];
        assert!(agent_inflight_load_from_events(&duplicate_harness, "rB321").is_err());
        let duplicate_wake = [request, b321_generic_request("beta", "wake-alpha")];
        assert!(agent_inflight_load_from_events(&duplicate_wake, "rB321").is_err());
    }

    #[test]
    fn b321_delivery_and_task_recorded_do_not_release_without_exact_closure() {
        let request = b321_generic_request("alpha", "wake-alpha");
        let delivery = crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({
                "attemptId": "B321-A0001", "role": "review", "agent": "alpha",
                "harness": "alpha", "wakeId": "wake-alpha", "bodyLen": 10
            }),
        );
        let recorded = crate::ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({"postMergeGates": "all-green"}),
        );
        assert_eq!(b321_generic_review_count(&[request.clone(), delivery]), 1);
        assert_eq!(b321_generic_review_count(&[request.clone(), recorded]), 1);

        let exact_terminal = crate::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({"attemptId": "B321-A0001"}),
        );
        assert_eq!(
            b321_generic_review_count(&[request.clone(), exact_terminal]),
            1
        );

        let mut forged_actor = crate::ledger::event(
            "AttemptBlocked",
            "forger",
            Some("B321"),
            Some("rB321"),
            serde_json::json!({"attemptId": "B321-A0001"}),
        );
        assert_eq!(
            b321_generic_review_count(&[request.clone(), forged_actor.clone()]),
            1
        );
        forged_actor.actor = "runtime:orch".to_string();
        forged_actor.round = None;
        assert_eq!(b321_generic_review_count(&[request, forged_actor]), 1);
    }

    #[test]
    fn b262_attempt_terminal_releases_all_exact_attempt_roles_only() {
        let events = [
            b246_review_request("B262", "B262-A0001", "primary", "executor-opencode"),
            b246_review_request("B262", "B262-A0001", "secondary", "executor-opencode"),
            b246_review_request("B262", "B262-A0002", "primary", "executor-opencode"),
            b246_review_request("B999", "B262-A0001", "primary", "executor-opencode"),
            b262_attempt_terminal("B262", "B262-A0001"),
        ];

        let remaining = b262_review_items(&events);
        assert_eq!(remaining.len(), 2);
        assert!(remaining
            .iter()
            .any(|item| item.task_id == "B262" && item.attempt_id == "B262-A0002"));
        assert!(remaining
            .iter()
            .any(|item| item.task_id == "B999" && item.attempt_id == "B262-A0001"));
    }

    #[test]
    fn b262_review_delivery_keeps_its_substantive_exact_release_contract() {
        let requested = b246_review_request("B262", "B262-A0001", "primary", "executor-opencode");
        for (label, delivered) in [
            (
                "blank",
                b262_review_delivery("B262", "B262-A0001", "primary", "executor-opencode", 0),
            ),
            (
                "wrong-attempt",
                b262_review_delivery("B262", "B262-A0002", "primary", "executor-opencode", 1),
            ),
            (
                "wrong-role",
                b262_review_delivery("B262", "B262-A0001", "secondary", "executor-opencode", 1),
            ),
            (
                "wrong-agent",
                b262_review_delivery(
                    "B262",
                    "B262-A0001",
                    "primary",
                    "executor-opencode-scout",
                    1,
                ),
            ),
        ] {
            assert_eq!(
                b262_review_items(&[requested.clone(), delivered]).len(),
                1,
                "{label} delivery must not release the review slot"
            );
        }

        let exact = b262_review_delivery("B262", "B262-A0001", "primary", "executor-opencode", 1);
        assert!(b262_review_items(&[requested, exact]).is_empty());
    }

    #[test]
    fn b310_task_recorded_releases_unanswered_substituted_formal_capacity() {
        let failed_primary =
            b246_review_request("B310", "B310-A0002", "primary", "executor-opencode");
        let unrelated = b246_review_request("B999", "B999-A0001", "primary", "executor-opencode");
        let substituted = crate::ledger::event(
            "ReviewSeatSubstituted",
            "runtime:orch",
            Some("B310"),
            Some("rB246"),
            serde_json::json!({
                "attemptId": "B310-A0002",
                "role": "primary",
                "fromAgent": "executor-opencode",
                "toAgent": "executor-dsh",
            }),
        );
        let recorded = crate::ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some("B310"),
            Some("rB246"),
            serde_json::json!({"postMergeGates": "all-green"}),
        );

        let remaining = b262_review_items(&[failed_primary, unrelated, substituted, recorded]);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].task_id, "B999");
        assert_eq!(remaining[0].attempt_id, "B999-A0001");
    }

    #[test]
    fn b246_exact_review_slot_replacement_admits_at_capacity_one() {
        let scheduling = b246_review_scheduling();
        let occupied = b246_review_request("B246", "B246-A0001", "primary", "executor-opencode");

        scheduling_admits_replacing_review_slot(
            &[occupied],
            "rB246",
            &scheduling,
            "executor-opencode",
            "primary-review",
            "B246",
            "B246-A0001",
            "primary",
        )
        .expect("the exact dead review slot is replaced rather than added at capacity one");
    }

    #[test]
    fn b246_replacement_refuses_every_identity_mismatch() {
        let scheduling = b246_review_scheduling();
        let occupied = b246_review_request("B246", "B246-A0001", "primary", "executor-opencode");

        for (label, agent, required_role, task_id, attempt_id, review_role) in [
            (
                "task",
                "executor-opencode",
                "primary-review",
                "B245",
                "B246-A0001",
                "primary",
            ),
            (
                "attempt",
                "executor-opencode",
                "primary-review",
                "B246",
                "B246-A0002",
                "primary",
            ),
            (
                "role",
                "executor-opencode",
                "secondary-review",
                "B246",
                "B246-A0001",
                "secondary",
            ),
            (
                "agent",
                "executor-opencode-scout",
                "primary-review",
                "B246",
                "B246-A0001",
                "primary",
            ),
        ] {
            let error = scheduling_admits_replacing_review_slot(
                std::slice::from_ref(&occupied),
                "rB246",
                &scheduling,
                agent,
                required_role,
                task_id,
                attempt_id,
                review_role,
            )
            .expect_err("a mismatched replacement identity must fail closed");
            assert!(
                error.contains("found=0"),
                "{label} mismatch did not fail at the exact-slot check: {error}"
            );
        }
    }

    #[test]
    fn b246_replacement_keeps_unrelated_same_agent_slot_counted() {
        let scheduling = b246_review_scheduling();
        let events = [
            b246_review_request("B246", "B246-A0001", "primary", "executor-opencode"),
            b246_review_request("B245", "B245-A0001", "secondary", "executor-opencode"),
        ];

        let error = scheduling_admits_replacing_review_slot(
            &events,
            "rB246",
            &scheduling,
            "executor-opencode",
            "primary-review",
            "B246",
            "B246-A0001",
            "primary",
        )
        .expect_err("an unrelated slot on the same agent must still consume capacity");
        assert!(error.contains("layer=agent"), "{error}");
        assert!(error.contains("B245:review/secondary"), "{error}");
        assert!(!error.contains("B246:review/primary"), "{error}");
    }

    #[test]
    fn b246_replacement_keeps_other_shared_domain_member_counted() {
        let scheduling = b246_review_scheduling();
        let events = [
            b246_review_request("B246", "B246-A0001", "primary", "executor-opencode"),
            b246_review_request("B245", "B245-A0001", "primary", "executor-opencode-scout"),
        ];

        let error = scheduling_admits_replacing_review_slot(
            &events,
            "rB246",
            &scheduling,
            "executor-opencode",
            "primary-review",
            "B246",
            "B246-A0001",
            "primary",
        )
        .expect_err("another member must continue to fill the shared quota domain");
        assert!(error.contains("layer=domain"), "{error}");
        assert!(error.contains("quotaDomain opencode"), "{error}");
        assert!(error.contains("B245:review/primary"), "{error}");
        assert!(!error.contains("B246:review/primary"), "{error}");
    }

    #[test]
    fn b246_ordinary_scheduling_still_refuses_the_occupied_slot() {
        let scheduling = b246_review_scheduling();
        let occupied = b246_review_request("B246", "B246-A0001", "primary", "executor-opencode");

        let error = scheduling_admits(
            &[occupied],
            "rB246",
            &scheduling,
            "executor-opencode",
            "primary-review",
        )
        .expect_err("ordinary admission must receive no replacement credit");
        assert!(error.contains("layer=agent"), "{error}");
        assert!(error.contains("B246:review/primary"), "{error}");
    }

    #[test]
    fn production_admission_uses_the_signed_shared_domain_projection() {
        let scheduling: crate::plan::IrScheduling = serde_yaml::from_str(
            r#"
allowedAgents: [executor-opencode, executor-opencode-scout]
capacities:
  executor-opencode: {agent: 1, quota: 1, roles: [implement]}
  executor-opencode-scout: {agent: 1, quota: 1, roles: [nongate-review]}
agentDomains:
  executor-opencode: opencode
  executor-opencode-scout: opencode
quotaDomains:
  opencode: {agent: 1, quota: 1}
"#,
        )
        .expect("shared-domain scheduling IR must parse");
        let occupied = crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B-live"),
            Some("rB234"),
            serde_json::json!({
                "agent": "executor-opencode",
                "attemptId": "B-live-A0001",
                "requestedAt": "2026-08-07T00:00:00Z"
            }),
        );

        let error = scheduling_admits(
            &[occupied],
            "rB234",
            &scheduling,
            "executor-opencode-scout",
            "nongate-review",
        )
        .expect_err("another instance in the signed shared domain must consume capacity");
        assert!(error.contains("layer=domain"), "{error}");
        assert!(error.contains("quotaDomain opencode"), "{error}");
        assert!(error.contains("B-live"), "{error}");
    }

    #[test]
    fn legacy_singleton_ir_keeps_agent_scoped_admission() {
        let scheduling: crate::plan::IrScheduling = serde_yaml::from_str(
            r#"
allowedAgents: [executor-desktop]
capacities:
  executor-desktop: {agent: 1, quota: 1, roles: [implement]}
"#,
        )
        .expect("legacy singleton scheduling IR must parse");

        scheduling_admits(&[], "rLegacy", &scheduling, "executor-desktop", "implement")
            .expect("empty legacy singleton lane must still admit");

        let occupied = crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B-legacy"),
            Some("rLegacy"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B-legacy-A0001",
                "requestedAt": "2026-08-07T00:00:00Z"
            }),
        );
        let error = scheduling_admits(
            &[occupied],
            "rLegacy",
            &scheduling,
            "executor-desktop",
            "implement",
        )
        .expect_err("legacy singleton capacity must remain agent-scoped");
        assert!(error.contains("layer=agent"), "{error}");
        assert!(error.contains("quotaDomain=executor-desktop"), "{error}");
    }
}
