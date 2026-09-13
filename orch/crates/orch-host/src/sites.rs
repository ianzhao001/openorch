//! Review-site lifecycle and conservative teardown.
//!
//! A review artifact is not a process-liveness receipt.  This module therefore
//! derives reclaimability exclusively from durable `WorkspaceLeased` /
//! `WorkspaceReleased` facts (or an exactly paired managed-wake termination).
//! Filesystem state is used only after that pure decision, while the ledger
//! lock is held and the decision has been recomputed from fresh bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use orch_core::EventRecord;
use serde::{Deserialize, Serialize};

use crate::{gitx, ledger};

pub const MANAGED_COMPLETION_RECEIPT: &str = "runtime:orch/managed-wake-terminated";
pub const SITE_RETIRED_EVENT_KIND: &str = "SiteRetired";
pub const QUARANTINE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Contract anchor for the B314 complete-journal ABA recovery.
///
/// A `CleanupJournal` whose `phase == "complete"` is the *receipt* of a
/// teardown already performed, never a veto on future teardown.  When the same
/// canonical `(SiteIdentity, generation)` worktree, target, or registry entry
/// reappears after that receipt was written (an ABA re-creation), `sites gc`
/// may redo the *physical* cleanup — but only after fresh ledger bytes inside
/// the same critical section contain exactly one same-round canonical
/// `WorkspaceReleased` or one same-round, validly anchored `SiteRetired` for
/// that generation.  A bare `ManagedWakeTerminated`/generic `Released` fold is
/// insufficient.  No different canonical historical lease may share either
/// path, even if that later generation has already folded released, and every
/// role-appropriate physical criterion must re-verify: canonical
/// relative path and all-ancestor no-symlink for every role; same git
/// common-dir, detached HEAD equal to the lease's `reviewedHead` for review
/// roles; the existing task-site reclaim criteria for implement roles; plus
/// tracked/staged cleanliness, bounded untracked quarantine, and exact-path
/// target/registry removal.  A successful replay keeps the historical journal
/// bytes bit-identical and mints no ledger event — no second release, no
/// retirement, no escalation — and a second run reaps nothing.  Ambiguous
/// evidence fails closed: the site, its target, the registry, and the ledger
/// all stay byte-identical.
///
/// The value pins version 1 of this contract; any behavioural change requires
/// a new version anchor and a new seeded contract.
pub const COMPLETE_JOURNAL_ABA_CONTRACT_V1: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SiteRole {
    Primary,
    Secondary,
    Nongate,
    /// Actorless schema-3 review invocation.
    Review,
    Implement,
}

impl SiteRole {
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "primary" => Ok(Self::Primary),
            "secondary" => Ok(Self::Secondary),
            "nongate" => Ok(Self::Nongate),
            "review" => Ok(Self::Review),
            "implement" => Ok(Self::Implement),
            other => Err(format!(
                "site role 只接受 primary/secondary/nongate/review/implement，收到 {other:?}"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Nongate => "nongate",
            Self::Review => "review",
            Self::Implement => "implement",
        }
    }

    pub const fn satisfies_formal_review_slot(self) -> bool {
        matches!(self, Self::Primary | Self::Secondary)
    }
}

impl std::fmt::Display for SiteRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SiteIdentity {
    pub task_id: String,
    pub role: SiteRole,
    pub agent: String,
    /// Stable logical identity without a generation suffix.
    pub site_id: String,
}

pub fn site_identity(task_id: &str, role: SiteRole, agent: &str) -> SiteIdentity {
    SiteIdentity {
        task_id: task_id.to_string(),
        role,
        agent: agent.to_string(),
        site_id: format!("{task_id}-{}-{agent}", role.as_str()),
    }
}

impl SiteIdentity {
    pub fn site_id_for(&self, generation: u32) -> String {
        format!("{}-g{generation:02}", self.site_id)
    }

    /// Return one past every generation ever mentioned for this identity.
    ///
    /// Releases and malformed duplicates still consume their number.  Reusing
    /// a number after teardown would make a crash replay unable to distinguish
    /// the old site from the newly provisioned one.
    pub fn next_generation(events: &[EventRecord], identity: &SiteIdentity) -> u32 {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind.as_str(),
                    "WorkspaceLeased" | "WorkspaceReleased" | SITE_RETIRED_EVENT_KIND
                )
            })
            .filter_map(|event| {
                let payload = event.payload.as_ref()?;
                let site_id = payload.get("siteId")?.as_str()?;
                let generation = payload.get("generation")?.as_u64()?;
                let generation = u32::try_from(generation).ok()?;
                (site_id == identity.site_id_for(generation)).then_some(generation)
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub site_id: String,
    pub generation: u32,
    pub task_id: String,
    pub attempt_id: String,
    pub role: SiteRole,
    pub agent: String,
    pub reviewed_head: String,
    /// Repository-relative worktree path.
    pub worktree: String,
    /// Repository-relative build-target path.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_id: Option<String>,
}

impl Site {
    pub fn identity(&self) -> SiteIdentity {
        site_identity(&self.task_id, self.role, &self.agent)
    }

    pub fn basename(&self) -> String {
        format!(
            "review-{}-{}-{}-g{:02}",
            self.attempt_id,
            self.role.as_str(),
            self.agent,
            self.generation
        )
    }

    pub fn expected_worktree(&self) -> String {
        match self.role {
            SiteRole::Implement => format!(".worktrees/{}", self.task_id),
            SiteRole::Primary | SiteRole::Secondary | SiteRole::Nongate | SiteRole::Review => {
                format!(".worktrees/{}", self.basename())
            }
        }
    }

    pub fn expected_target(&self) -> String {
        match self.role {
            SiteRole::Implement => format!(".worktrees/{}/orch/target", self.task_id),
            SiteRole::Primary | SiteRole::Secondary | SiteRole::Nongate | SiteRole::Review => {
                format!("orch/target/{}", self.basename())
            }
        }
    }

    fn has_production_path_contract(&self) -> bool {
        self.worktree == self.expected_worktree() && self.target == self.expected_target()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseState {
    Absent,
    Active {
        site: Option<Site>,
        reason: String,
    },
    Released {
        site: Site,
        completion_receipt: String,
    },
}

fn payload_string<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
}

fn payload_u32(event: &EventRecord, key: &str) -> Option<u32> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn raw_site_key(event: &EventRecord) -> Option<(String, u32)> {
    Some((
        payload_string(event, "siteId")?.to_string(),
        payload_u32(event, "generation")?,
    ))
}

fn parse_lease(event: &EventRecord) -> std::result::Result<Site, String> {
    if event.kind != "WorkspaceLeased" || event.actor != "runtime:orch" {
        return Err("lease kind/actor 非 canonical".to_string());
    }
    let task_id = event
        .task_id
        .as_deref()
        .filter(|value| safe_component(value))
        .ok_or_else(|| "lease 缺安全 taskId".to_string())?;
    let required = |key: &str| {
        payload_string(event, key)
            .filter(|value| safe_component(value))
            .ok_or_else(|| format!("lease 缺安全 payload.{key}"))
    };
    let site_id = required("siteId")?;
    let generation = payload_u32(event, "generation")
        .filter(|value| *value > 0)
        .ok_or_else(|| "lease generation 必须是正 u32".to_string())?;
    let attempt_id = required("attemptId")?;
    let role = SiteRole::parse(required("role")?)?;
    let agent = required("agent")?;
    let reviewed_head = payload_string(event, "reviewedHead")
        .filter(|value| full_sha(value))
        .ok_or_else(|| "lease reviewedHead 必须是完整 SHA".to_string())?;
    let identity = site_identity(task_id, role, agent);
    if site_id != identity.site_id_for(generation) {
        return Err("lease siteId 与 task/role/agent/generation 不一致".to_string());
    }
    let paths = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("paths"))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "lease 缺 paths object".to_string())?;
    let worktree = paths
        .get("worktree")
        .and_then(serde_json::Value::as_str)
        .filter(|value| safe_relative_path(value))
        .ok_or_else(|| "lease paths.worktree 非安全相对路径".to_string())?;
    let target = paths
        .get("target")
        .and_then(serde_json::Value::as_str)
        .filter(|value| safe_relative_path(value))
        .ok_or_else(|| "lease paths.target 非安全相对路径".to_string())?;
    let wake_id = payload_string(event, "wakeId")
        .filter(|value| safe_component(value))
        .map(str::to_string);
    Ok(Site {
        site_id: site_id.to_string(),
        generation,
        task_id: task_id.to_string(),
        attempt_id: attempt_id.to_string(),
        role,
        agent: agent.to_string(),
        reviewed_head: reviewed_head.to_string(),
        worktree: worktree.to_string(),
        target: target.to_string(),
        wake_id,
    })
}

fn release_matches_site(event: &EventRecord, site: &Site) -> bool {
    if event.kind != "WorkspaceReleased" || event.actor != "runtime:orch" {
        return false;
    }
    event.task_id.as_deref() == Some(site.task_id.as_str())
        && payload_string(event, "siteId") == Some(site.site_id.as_str())
        && payload_u32(event, "generation") == Some(site.generation)
        && payload_string(event, "attemptId") == Some(site.attempt_id.as_str())
        && payload_string(event, "role") == Some(site.role.as_str())
        && payload_string(event, "agent") == Some(site.agent.as_str())
        && payload_string(event, "completionReceipt") == Some(MANAGED_COMPLETION_RECEIPT)
        && match (site.wake_id.as_deref(), payload_string(event, "wakeId")) {
            (Some(leased), Some(released)) => leased == released,
            (None, None) => true,
            _ => false,
        }
}

fn retirement_anchor_matches(
    events: &[EventRecord],
    retirement: &EventRecord,
    site: &Site,
) -> bool {
    let Some(anchor_id) = payload_string(retirement, "retireEventId") else {
        return false;
    };
    let anchors = events
        .iter()
        .filter(|event| event.event_id == anchor_id)
        .collect::<Vec<_>>();
    if anchors.len() != 1 {
        return false;
    }
    let anchor = anchors[0];
    if anchor.actor != "runtime:orch" || anchor.round != retirement.round {
        return false;
    }
    match anchor.kind.as_str() {
        "TaskRecorded" => anchor.task_id.as_deref() == Some(site.task_id.as_str()),
        "RoundClosed" => anchor.task_id.is_none(),
        "ActionRejected" if payload_string(retirement, "trigger") == Some("pre-spawn-rejected") => {
            let Some(wake_id) = site.wake_id.as_deref() else {
                return false;
            };
            let anchor_position = events
                .iter()
                .position(|event| event.event_id == anchor.event_id);
            let retirement_position = events
                .iter()
                .position(|event| event.event_id == retirement.event_id);
            anchor.task_id.as_deref() == Some(site.task_id.as_str())
                && payload_string(anchor, "actionId") == Some(wake_id)
                && payload_string(anchor, "operation") == Some("wake")
                && payload_string(anchor, "attemptId") == Some(site.attempt_id.as_str())
                && anchor
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptNo"))
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|value| value > 0)
                && anchor
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("exitCode"))
                    .and_then(serde_json::Value::as_i64)
                    == Some(2)
                && anchor
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("alert"))
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                && payload_string(anchor, "reason").is_some_and(|value| !value.is_empty())
                && anchor_position
                    .zip(retirement_position)
                    .is_some_and(|(anchor, retirement)| anchor + 1 == retirement)
                && !events.iter().any(|event| {
                    matches!(event.kind.as_str(), "WakeIssued" | "ReviewRequested")
                        && payload_string(event, "wakeId") == Some(wake_id)
                })
        }
        _ => false,
    }
}

fn retirement_matches_site(events: &[EventRecord], retirement: &EventRecord, site: &Site) -> bool {
    if retirement.kind != SITE_RETIRED_EVENT_KIND || retirement.actor != "runtime:orch" {
        return false;
    }
    retirement.task_id.as_deref() == Some(site.task_id.as_str())
        && payload_string(retirement, "siteId") == Some(site.site_id.as_str())
        && payload_u32(retirement, "generation") == Some(site.generation)
        && payload_string(retirement, "taskId") == Some(site.task_id.as_str())
        && payload_string(retirement, "attemptId") == Some(site.attempt_id.as_str())
        && payload_string(retirement, "role") == Some(site.role.as_str())
        && payload_string(retirement, "agent") == Some(site.agent.as_str())
        && matches!(
            payload_string(retirement, "trigger"),
            Some("task-recorded" | "round-close" | "manual" | "pre-spawn-rejected")
        )
        && match (
            site.wake_id.as_deref(),
            payload_string(retirement, "wakeId"),
        ) {
            (Some(leased), Some(retired)) => leased == retired,
            (None, None) => true,
            _ => false,
        }
        && retirement_anchor_matches(events, retirement, site)
}

/// Whether one review lease was atomically retired after a proven no-spawn
/// rejection. Such a lease is terminal infrastructure evidence, not a review
/// request, and must not keep the attempt's request identity set open.
pub(crate) fn lease_is_pre_spawn_rejected(events: &[EventRecord], lease: &EventRecord) -> bool {
    let Ok(site) = parse_lease(lease) else {
        return false;
    };
    let retirements = events
        .iter()
        .filter(|event| {
            event.kind == SITE_RETIRED_EVENT_KIND
                && payload_string(event, "trigger") == Some("pre-spawn-rejected")
                && raw_site_key(event) == Some((site.site_id.clone(), site.generation))
        })
        .collect::<Vec<_>>();
    matches!(retirements.as_slice(), [retirement] if retirement_matches_site(events, retirement, &site))
}

fn complete_journal_release_is_explicit(events: &[EventRecord], round: &str, site: &Site) -> bool {
    if !site_evidence_is_unambiguous(events) {
        return false;
    }
    let key = (site.site_id.clone(), site.generation);
    let leases = events
        .iter()
        .filter(|event| event.kind == "WorkspaceLeased" && raw_site_key(event) == Some(key.clone()))
        .collect::<Vec<_>>();
    let [lease] = leases.as_slice() else {
        return false;
    };
    if lease.round.as_deref() != Some(round)
        || !matches!(parse_lease(lease), Ok(parsed) if parsed == *site)
    {
        return false;
    }
    let releases = events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceReleased" && raw_site_key(event) == Some(key.clone())
        })
        .collect::<Vec<_>>();
    let retirements = events
        .iter()
        .filter(|event| {
            event.kind == SITE_RETIRED_EVENT_KIND && raw_site_key(event) == Some(key.clone())
        })
        .collect::<Vec<_>>();
    match (releases.as_slice(), retirements.as_slice()) {
        ([release], []) => {
            release.round.as_deref() == Some(round)
                && release_matches_site(release, site)
                && release_termination_anchor_is_same_round(events, release, lease, round, site)
        }
        ([], [retirement]) => {
            retirement.round.as_deref() == Some(round)
                && retirement_matches_site(events, retirement, site)
        }
        _ => false,
    }
}

fn release_termination_anchor_is_same_round(
    events: &[EventRecord],
    release: &EventRecord,
    lease: &EventRecord,
    round: &str,
    site: &Site,
) -> bool {
    let Some(termination_event_id) = payload_string(release, "terminationEventId") else {
        return true;
    };
    let terminations = events
        .iter()
        .filter(|event| event.event_id == termination_event_id)
        .collect::<Vec<_>>();
    let [termination] = terminations.as_slice() else {
        return false;
    };
    lease.round.as_deref() == Some(round)
        && release.round.as_deref() == Some(round)
        && termination.round.as_deref() == Some(round)
        && managed_termination_matches(termination, site)
}

fn complete_journal_paths_are_exclusive(events: &[EventRecord], site: &Site) -> bool {
    events
        .iter()
        .filter(|event| event.kind == "WorkspaceLeased")
        .filter_map(|event| parse_lease(event).ok())
        .all(|candidate| {
            (candidate.site_id == site.site_id && candidate.generation == site.generation)
                || (candidate.worktree != site.worktree && candidate.target != site.target)
        })
}

fn site_evidence_is_unambiguous(events: &[EventRecord]) -> bool {
    let leases = events
        .iter()
        .filter(|event| event.kind == "WorkspaceLeased")
        .collect::<Vec<_>>();
    if leases.iter().any(|event| parse_lease(event).is_err()) {
        return false;
    }
    let mut lease_counts = BTreeMap::<(String, u32), usize>::new();
    for lease in &leases {
        let Some(key) = raw_site_key(lease) else {
            return false;
        };
        *lease_counts.entry(key).or_default() += 1;
    }
    if lease_counts.values().any(|count| *count != 1) {
        return false;
    }

    let mut release_counts = BTreeMap::<(String, u32), usize>::new();
    for release in events
        .iter()
        .filter(|event| event.kind == "WorkspaceReleased")
    {
        let Some(key) = raw_site_key(release) else {
            return false;
        };
        let Some(lease) = leases
            .iter()
            .find(|lease| raw_site_key(lease) == Some(key.clone()))
        else {
            return false;
        };
        let Ok(site) = parse_lease(lease) else {
            return false;
        };
        if !release_matches_site(release, &site) {
            return false;
        }
        if let Some(termination_event_id) = payload_string(release, "terminationEventId") {
            let terminations = events
                .iter()
                .filter(|event| managed_termination_matches(event, &site))
                .collect::<Vec<_>>();
            if terminations.len() != 1 || terminations[0].event_id != termination_event_id {
                return false;
            }
        }
        *release_counts.entry(key).or_default() += 1;
    }
    if release_counts.values().any(|count| *count > 1) {
        return false;
    }

    let mut retirement_counts = BTreeMap::<(String, u32), usize>::new();
    for retirement in events
        .iter()
        .filter(|event| event.kind == SITE_RETIRED_EVENT_KIND)
    {
        let Some(key) = raw_site_key(retirement) else {
            return false;
        };
        let matching_leases = leases
            .iter()
            .filter(|lease| raw_site_key(lease) == Some(key.clone()))
            .collect::<Vec<_>>();
        if matching_leases.len() != 1 {
            return false;
        }
        let Ok(site) = parse_lease(matching_leases[0]) else {
            return false;
        };
        if !retirement_matches_site(events, retirement, &site) {
            return false;
        }
        *retirement_counts.entry(key).or_default() += 1;
    }
    !retirement_counts.values().any(|count| *count > 1)
}

fn managed_termination_matches(event: &EventRecord, site: &Site) -> bool {
    let Some(wake_id) = site.wake_id.as_deref() else {
        return false;
    };
    event.kind == "ManagedWakeTerminated"
        && event.actor == "runtime:orch"
        && event.task_id.as_deref() == Some(site.task_id.as_str())
        && payload_string(event, "wakeId") == Some(wake_id)
        && payload_string(event, "agent") == Some(site.agent.as_str())
        && event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("managedScopeTerminated"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

impl LeaseState {
    pub fn of(events: &[EventRecord], identity: &SiteIdentity, generation: u32) -> Self {
        let key = identity.site_id_for(generation);
        let leases = events
            .iter()
            .filter(|event| {
                event.kind == "WorkspaceLeased"
                    && raw_site_key(event).as_ref() == Some(&(key.clone(), generation))
            })
            .collect::<Vec<_>>();
        if leases.is_empty() {
            return Self::Absent;
        }
        if !site_evidence_is_unambiguous(events) {
            return Self::Active {
                site: leases.first().and_then(|event| parse_lease(event).ok()),
                reason: "workspace lifecycle evidence 存在未配对/重复/身份缺失，fail-closed"
                    .to_string(),
            };
        }
        if leases.len() != 1 {
            return Self::Active {
                site: leases.first().and_then(|event| parse_lease(event).ok()),
                reason: format!("lease 计数={}，必须恰好一条", leases.len()),
            };
        }
        let site = match parse_lease(leases[0]) {
            Ok(site) if site.identity() == *identity && site.generation == generation => site,
            Ok(site) => {
                return Self::Active {
                    site: Some(site),
                    reason: "lease identity 漂移".to_string(),
                }
            }
            Err(reason) => return Self::Active { site: None, reason },
        };
        let releases = events
            .iter()
            .filter(|event| {
                event.kind == "WorkspaceReleased"
                    && raw_site_key(event).as_ref() == Some(&(key.clone(), generation))
            })
            .collect::<Vec<_>>();
        if releases.len() > 1 {
            return Self::Active {
                site: Some(site),
                reason: format!("release 计数={}，绑定歧义", releases.len()),
            };
        }
        if let Some(release) = releases.first() {
            if release_matches_site(release, &site) {
                return Self::Released {
                    site,
                    completion_receipt: MANAGED_COMPLETION_RECEIPT.to_string(),
                };
            }
            return Self::Active {
                site: Some(site),
                reason: "release 缺失、身份漂移或无 runtime completion receipt".to_string(),
            };
        }
        let retirements = events
            .iter()
            .filter(|event| {
                event.kind == SITE_RETIRED_EVENT_KIND
                    && raw_site_key(event).as_ref() == Some(&(key.clone(), generation))
            })
            .collect::<Vec<_>>();
        if retirements.len() > 1 {
            return Self::Active {
                site: Some(site),
                reason: format!("SiteRetired 计数={}，绑定歧义", retirements.len()),
            };
        }
        if let Some(retirement) = retirements.first() {
            if retirement_matches_site(events, retirement, &site) {
                return Self::Released {
                    site,
                    completion_receipt: format!("SiteRetired:{}", retirement.event_id),
                };
            }
            return Self::Active {
                site: Some(site),
                reason: "SiteRetired 缺失、身份漂移或 retireEventId 未锚定授权事件".to_string(),
            };
        }
        let terminations = events
            .iter()
            .filter(|event| managed_termination_matches(event, &site))
            .collect::<Vec<_>>();
        if terminations.len() == 1 {
            return Self::Released {
                site,
                completion_receipt: format!("ManagedWakeTerminated:{}", terminations[0].event_id),
            };
        }
        Self::Active {
            site: Some(site),
            reason: if terminations.is_empty() {
                "未见配对 release/managed termination".to_string()
            } else {
                "managed termination 重复，绑定歧义".to_string()
            },
        }
    }
}

fn managed_wake_is_pending(events: &[EventRecord], site: &Site) -> bool {
    let Some(wake_id) = site.wake_id.as_deref() else {
        return false;
    };
    events.iter().any(|event| {
        event.kind == "WakeIssued"
            && event.actor == "runtime:orch"
            && event.task_id.as_deref() == Some(site.task_id.as_str())
            && payload_string(event, "wakeId") == Some(wake_id)
            && payload_string(event, "agent") == Some(site.agent.as_str())
            && payload_string(event, "backendState") == Some("pending")
            && payload_string(event, "controlWakeId") == Some(wake_id)
    })
}

fn lease_is_declared_preserve(lease: &EventRecord) -> bool {
    lease
        .payload
        .as_ref()
        .and_then(|payload| payload.get("declaredPreserve"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// Build task-record retirement facts from ledger evidence only.
///
/// The caller places the returned facts in the same checked batch as the
/// `TaskRecorded` anchor named by `anchor_event_id`.  A managed wake without
/// authenticated terminal evidence is deliberately left to reconciliation,
/// and a planner-declared preserve remains active for explicit late review.
pub fn retire_task_sites(
    events: &[EventRecord],
    task_id: &str,
    anchor_event_id: &str,
) -> Vec<EventRecord> {
    if !safe_component(task_id)
        || anchor_event_id.is_empty()
        || !site_evidence_is_unambiguous(events)
    {
        return Vec::new();
    }
    events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceLeased"
                && event.task_id.as_deref() == Some(task_id)
                && !lease_is_declared_preserve(event)
        })
        .filter_map(|lease| {
            let site = parse_lease(lease).ok()?;
            if !matches!(
                LeaseState::of(events, &site.identity(), site.generation),
                LeaseState::Active { .. }
            ) || managed_wake_is_pending(events, &site)
            {
                return None;
            }
            Some(ledger::event(
                SITE_RETIRED_EVENT_KIND,
                "runtime:orch",
                Some(&site.task_id),
                lease.round.as_deref(),
                serde_json::json!({
                    "siteId": site.site_id,
                    "generation": site.generation,
                    "taskId": site.task_id,
                    "attemptId": site.attempt_id,
                    "role": site.role.as_str(),
                    "agent": site.agent,
                    "wakeId": site.wake_id,
                    "trigger": "task-recorded",
                    "retireEventId": anchor_event_id,
                }),
            ))
        })
        .collect()
}

fn lease_identities(events: &[EventRecord]) -> BTreeSet<(SiteIdentity, u32)> {
    events
        .iter()
        .filter(|event| event.kind == "WorkspaceLeased")
        .filter_map(|event| parse_lease(event).ok())
        .map(|site| (site.identity(), site.generation))
        .collect()
}

fn active_sites(events: &[EventRecord]) -> Vec<Site> {
    lease_identities(events)
        .into_iter()
        .filter_map(
            |(identity, generation)| match LeaseState::of(events, &identity, generation) {
                LeaseState::Active {
                    site: Some(site), ..
                } => Some(site),
                LeaseState::Absent
                | LeaseState::Released { .. }
                | LeaseState::Active { site: None, .. } => None,
            },
        )
        .collect()
}

pub(crate) fn active_sites_checked(
    events: &[EventRecord],
) -> std::result::Result<Vec<Site>, String> {
    if !site_evidence_is_unambiguous(events) {
        return Err("workspace lease evidence is ambiguous".to_string());
    }
    Ok(active_sites(events))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidueDisposition {
    Proceed,
    Quarantine { bytes: u64, limit: u64 },
    RefuseAndEscalate { bytes: u64, limit: u64 },
}

impl ResidueDisposition {
    pub fn for_untracked(paths: &[&str], bytes: u64) -> Self {
        if paths.is_empty() {
            Self::Proceed
        } else if bytes <= QUARANTINE_LIMIT_BYTES {
            Self::Quarantine {
                bytes,
                limit: QUARANTINE_LIMIT_BYTES,
            }
        } else {
            Self::RefuseAndEscalate {
                bytes,
                limit: QUARANTINE_LIMIT_BYTES,
            }
        }
    }
}

fn validate_site_request(
    task_id: &str,
    attempt_id: &str,
    role: SiteRole,
    agent: &str,
    reviewed_head: &str,
    wake_id: &str,
) -> Result<()> {
    for (label, value) in [
        ("taskId", task_id),
        ("attemptId", attempt_id),
        ("agent", agent),
        ("wakeId", wake_id),
    ] {
        if !safe_component(value) {
            bail!("site lease {label} 不是安全 component: {value:?}");
        }
    }
    if !attempt_id.starts_with(&format!("{task_id}-A")) {
        bail!("site lease attemptId 未绑定 taskId");
    }
    if !full_sha(reviewed_head) {
        bail!("site lease reviewedHead 必须是完整 SHA");
    }
    let _ = role;
    Ok(())
}

fn planned_site(
    task_id: &str,
    attempt_id: &str,
    role: SiteRole,
    agent: &str,
    reviewed_head: &str,
    wake_id: &str,
    generation: u32,
) -> Site {
    let identity = site_identity(task_id, role, agent);
    let basename = format!(
        "review-{attempt_id}-{}-{agent}-g{generation:02}",
        role.as_str()
    );
    Site {
        site_id: identity.site_id_for(generation),
        generation,
        task_id: task_id.to_string(),
        attempt_id: attempt_id.to_string(),
        role,
        agent: agent.to_string(),
        reviewed_head: reviewed_head.to_string(),
        worktree: format!(".worktrees/{basename}"),
        target: format!("orch/target/{basename}"),
        wake_id: Some(wake_id.to_string()),
    }
}

fn workspace_leased_event(round: &str, site: &Site) -> EventRecord {
    ledger::event(
        "WorkspaceLeased",
        "runtime:orch",
        Some(&site.task_id),
        Some(round),
        serde_json::json!({
            "siteId": site.site_id,
            "generation": site.generation,
            "attemptId": site.attempt_id,
            "role": site.role.as_str(),
            "agent": site.agent,
            "reviewedHead": site.reviewed_head,
            "wakeId": site.wake_id,
            "paths": {
                "worktree": site.worktree,
                "target": site.target,
            },
        }),
    )
}

fn require_open_schema3_review_site_generation(
    root: &Path,
    round: &str,
    events: &[EventRecord],
) -> Result<()> {
    if crate::current_round(root)? != round {
        bail!("review site lease round is not current");
    }
    if orch_core::fold(events).round_closed {
        bail!("review site lease refuses a closed round");
    }
    if crate::round::contract_schema_from_events(events, round)?
        != Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        bail!("review site lease only writes the current open schema 3 round");
    }
    Ok(())
}

/// Allocate and durably lease one generation while holding the round ledger
/// lock.  `provision` executes inside that same lock, which serializes it with
/// teardown's fresh read and closes the late-lease deletion race. New leases
/// exist only for the actorless `review` role in the current open schema-3
/// round; historical primary/secondary/nongate roles remain decode-only.
pub fn lease_review_site_with<F>(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    role: SiteRole,
    agent: &str,
    reviewed_head: &str,
    wake_id: &str,
    provision: F,
) -> Result<Site>
where
    F: FnOnce(&Site) -> Result<()>,
{
    if role != SiteRole::Review {
        bail!("review site lease only accepts the schema 3 review role");
    }
    validate_site_request(task_id, attempt_id, role, agent, reviewed_head, wake_id)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取 site lease 账本失败: {}", ledger_path.display()))?;
    if !read.bad_lines.is_empty() {
        bail!("site lease 拒绝坏账本");
    }
    require_open_schema3_review_site_generation(root, round, &read.events)?;

    let identity = site_identity(task_id, role, agent);
    let mut selected = None::<Site>;
    let mut provision = Some(provision);
    ledger::append_checked(root, round, |events| {
        require_open_schema3_review_site_generation(root, round, events)?;
        let matching = lease_identities(events)
            .into_iter()
            .filter(|(candidate, _)| candidate == &identity)
            .filter_map(|(candidate, generation)| {
                match LeaseState::of(events, &candidate, generation) {
                    LeaseState::Active {
                        site: Some(site), ..
                    } if site.attempt_id == attempt_id
                        && site.reviewed_head == reviewed_head
                        && site.wake_id.as_deref() == Some(wake_id) =>
                    {
                        Some(site)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            bail!("same site request has multiple active generations");
        }
        if let Some(site) = matching.into_iter().next() {
            provision.take().expect("site provision closure runs once")(&site)?;
            selected = Some(site);
            return Ok(Vec::new());
        }

        let generation = SiteIdentity::next_generation(events, &identity);
        if generation == 0
            || (generation == u32::MAX
                && events.iter().any(|event| {
                    raw_site_key(event).is_some_and(|(site_id, value)| {
                        site_id == identity.site_id_for(value) && value == u32::MAX
                    })
                }))
        {
            bail!("review site generation overflow");
        }
        let site = planned_site(
            task_id,
            attempt_id,
            role,
            agent,
            reviewed_head,
            wake_id,
            generation,
        );
        provision.take().expect("site provision closure runs once")(&site)?;
        selected = Some(site.clone());
        Ok(vec![workspace_leased_event(round, &site)])
    })?;
    selected.context("site lease decision returned without a selected site")
}

/// Build a release fact only from an authenticated managed termination.  The
/// caller appends this adjacent to the termination event in the same checked
/// ledger transaction.
pub fn workspace_release_for_termination(
    events: &[EventRecord],
    round: &str,
    termination: &EventRecord,
) -> Result<Option<EventRecord>> {
    if termination.kind != "ManagedWakeTerminated"
        || termination.actor != "runtime:orch"
        || termination.round.as_deref() != Some(round)
    {
        bail!("WorkspaceReleased requires a canonical ManagedWakeTerminated");
    }
    let managed_scope_terminated = termination
        .payload
        .as_ref()
        .and_then(|payload| payload.get("managedScopeTerminated"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if !managed_scope_terminated {
        return Ok(None);
    }
    let wake_id =
        payload_string(termination, "wakeId").context("ManagedWakeTerminated missing wakeId")?;
    let leases = events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceLeased" && payload_string(event, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    if leases.is_empty() {
        return Ok(None);
    }
    if leases.len() != 1 {
        // The termination fact remains valuable even when lifecycle evidence
        // is ambiguous.  Refuse only the release arm so the site stays active.
        return Ok(None);
    }
    let Ok(site) = parse_lease(leases[0]) else {
        return Ok(None);
    };
    if termination.task_id.as_deref() != Some(site.task_id.as_str())
        || payload_string(termination, "agent") != Some(site.agent.as_str())
    {
        return Ok(None);
    }
    let existing = events
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceReleased"
                && raw_site_key(event) == Some((site.site_id.clone(), site.generation))
        })
        .count();
    if existing > 1 {
        bail!("workspace release ledger already contains duplicates");
    }
    if existing == 1 {
        return Ok(None);
    }
    Ok(Some(ledger::event(
        "WorkspaceReleased",
        "runtime:orch",
        Some(&site.task_id),
        Some(round),
        serde_json::json!({
            "siteId": site.site_id,
            "generation": site.generation,
            "attemptId": site.attempt_id,
            "role": site.role.as_str(),
            "agent": site.agent,
            "wakeId": wake_id,
            "completionReceipt": MANAGED_COMPLETION_RECEIPT,
            "terminationEventId": termination.event_id,
        }),
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacySiteReport {
    pub worktree: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryInvariant {
    pub registered: usize,
    pub active_leases: usize,
    pub declared_preserves: usize,
    pub prunable: usize,
    pub holds: bool,
    pub missing: Vec<String>,
    pub unexpected: Vec<String>,
}

pub fn worktree_registry_invariant(
    registered: &[String],
    active_leases: &[Site],
    declared_preserves: &[String],
) -> RegistryInvariant {
    worktree_registry_invariant_with_prunable(registered, active_leases, declared_preserves, &[])
}

fn worktree_registry_invariant_with_prunable(
    registered: &[String],
    active_leases: &[Site],
    declared_preserves: &[String],
    prunable: &[String],
) -> RegistryInvariant {
    let registered = registered.iter().cloned().collect::<BTreeSet<_>>();
    let active = active_leases
        .iter()
        .map(|site| site.worktree.clone())
        .collect::<BTreeSet<_>>();
    let preserves = declared_preserves.iter().cloned().collect::<BTreeSet<_>>();
    let expected = active.union(&preserves).cloned().collect::<BTreeSet<_>>();
    RegistryInvariant {
        registered: registered.len(),
        active_leases: active.len(),
        declared_preserves: preserves.len(),
        prunable: prunable.len(),
        holds: registered == expected && prunable.is_empty(),
        missing: expected.difference(&registered).cloned().collect(),
        unexpected: registered.difference(&expected).cloned().collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReapOutcome {
    pub removed: Vec<String>,
    pub quarantined: Vec<String>,
    pub refused: Vec<String>,
    pub refused_details: Vec<(String, String)>,
    pub target_failures: Vec<String>,
    pub legacy_reports: Vec<LegacySiteReport>,
    pub registry_invariant: Option<RegistryInvariant>,
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SiteReapReport {
    pub reaped: Vec<String>,
    pub refused: Vec<(String, String)>,
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReclaimReportState {
    Reclaimable,
    Active,
    Legacy,
}

impl ReclaimReportState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Reclaimable => "可回收",
            Self::Active => "活跃",
            Self::Legacy => "legacy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReclaimReportEntry {
    pub state: ReclaimReportState,
    pub site_id: String,
    pub worktree: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CleanupJournal {
    version: u32,
    round: String,
    site: Site,
    phase: String,
    quarantine: Option<String>,
    note: Option<String>,
}

fn journal_path(root: &Path, round: &str, site: &Site) -> PathBuf {
    root.join("coordination/runtime/site-cleanup")
        .join(round)
        .join(format!("{}.json", site.site_id))
}

fn write_journal(root: &Path, round: &str, journal: &CleanupJournal) -> Result<()> {
    let path = journal_path(root, round, &journal.site);
    let parent = path.parent().context("site cleanup journal lacks parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("创建 site cleanup journal 目录失败: {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        journal.site.site_id,
        std::process::id(),
        ulid::Ulid::new()
    ));
    let bytes = serde_json::to_vec_pretty(journal)?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("创建 site journal temp 失败: {}", temporary.display()))?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)
            .with_context(|| format!("原子替换 site journal 失败: {}", path.display()))?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_journal(root: &Path, round: &str, site: &Site) -> Result<Option<CleanupJournal>> {
    let path = journal_path(root, round, site);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: CleanupJournal = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析 site cleanup journal 失败: {}", path.display()))?;
    if journal.version != 1 || journal.round != round || journal.site != *site {
        bail!(
            "site cleanup journal identity/version drift: {}",
            path.display()
        );
    }
    Ok(Some(journal))
}

fn no_symlink_ancestors(root: &Path, relative: &str) -> Result<PathBuf> {
    if !safe_relative_path(relative) {
        bail!("site path 不是安全相对路径: {relative:?}");
    }
    let mut cursor = root.to_path_buf();
    let components = Path::new(relative).components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            bail!("site path 含非 Normal component");
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("site path 含 symlink component: {}", cursor.display())
            }
            Ok(metadata) if index + 1 < components.len() && !metadata.is_dir() => {
                bail!("site path ancestor 不是目录: {}", cursor.display())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(root.join(relative))
}

fn untracked_inventory(worktree: &Path) -> Result<(Vec<String>, u64)> {
    let output = maintenance_git(
        worktree,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let paths = output
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| std::str::from_utf8(p).map(PathBuf::from))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut bytes = 0_u64;
    let mut rendered = Vec::with_capacity(paths.len());
    for relative in paths {
        let text = relative
            .to_str()
            .context("untracked path 不是 UTF-8")?
            .to_string();
        if !safe_relative_path(&text) {
            bail!("untracked path 逃逸现场: {text:?}");
        }
        let path = worktree.join(&relative);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("读取 untracked residue 失败: {}", path.display()))?;
        bytes = bytes
            .checked_add(metadata.len())
            .context("untracked residue 字节计数溢出")?;
        rendered.push(text);
    }
    Ok((rendered, bytes))
}

fn quarantine_entries(
    root: &Path,
    round: &str,
    site: &Site,
    worktree: &Path,
    entries: &[String],
) -> Result<PathBuf> {
    let quarantine = root
        .join("coordination/runtime/site-quarantine")
        .join(round)
        .join(&site.site_id);
    let mut moves = Vec::with_capacity(entries.len());
    for relative in entries {
        let source = worktree.join(relative);
        let destination = quarantine.join(relative);
        no_symlink_ancestors(
            root,
            destination
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF8 quarantine path")?,
        )?;
        let parent = destination
            .parent()
            .context("quarantine destination lacks parent")?
            .to_path_buf();
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                bail!(
                    "quarantine destination 已存在，拒绝覆盖: {}",
                    destination.display()
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        moves.push((source, destination, parent));
    }
    // Preflight every destination before moving the first source.  In
    // particular, a collision on a later residue must not leave an immutable
    // complete-journal replay with only an earlier residue quarantined.
    for (_, _, parent) in &moves {
        fs::create_dir_all(parent)?;
    }
    for (_, destination, _) in &moves {
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                bail!(
                    "quarantine destination 已存在，拒绝覆盖: {}",
                    destination.display()
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut moved: Vec<(&PathBuf, &PathBuf)> = Vec::new();
    for (source, destination, _) in &moves {
        if let Err(error) = fs::rename(source, destination) {
            let mut rollback_failures = Vec::new();
            for (moved_source, moved_destination) in moved.iter().rev() {
                if let Err(rollback) = fs::rename(moved_destination, moved_source) {
                    rollback_failures.push(format!(
                        "{} -> {}: {rollback}",
                        moved_destination.display(),
                        moved_source.display()
                    ));
                }
            }
            if !rollback_failures.is_empty() {
                bail!(
                    "隔离 untracked residue 失败且回滚不完整: {} -> {}: {error}; rollback={rollback_failures:?}",
                    source.display(),
                    destination.display()
                );
            }
            return Err(error).with_context(|| {
                format!(
                    "隔离 untracked residue 失败: {} -> {}",
                    source.display(),
                    destination.display()
                )
            });
        }
        moved.push((source, destination));
    }
    Ok(quarantine)
}

enum ReapOne {
    Removed,
    AlreadyComplete,
    Refused { reason: String, append_event: bool },
    TargetFailed { reason: String, append_event: bool },
}

impl ReapOne {
    fn refused(reason: impl Into<String>) -> Self {
        Self::Refused {
            reason: reason.into(),
            append_event: true,
        }
    }

    fn quiet_refusal(reason: impl Into<String>) -> Self {
        Self::Refused {
            reason: reason.into(),
            append_event: false,
        }
    }

    fn target_failed(reason: impl Into<String>) -> Self {
        Self::TargetFailed {
            reason: reason.into(),
            append_event: true,
        }
    }

    fn without_event(self) -> Self {
        match self {
            Self::Refused { reason, .. } => Self::quiet_refusal(reason),
            Self::TargetFailed { reason, .. } => Self::TargetFailed {
                reason,
                append_event: false,
            },
            other => other,
        }
    }
}

fn reclaim_boundary_bytes(worktree: &Path, target: &Path) -> Result<u64> {
    let paths = if target.starts_with(worktree) {
        vec![worktree]
    } else if worktree.starts_with(target) {
        vec![target]
    } else {
        vec![worktree, target]
    };
    paths.into_iter().try_fold(0_u64, |total, path| {
        let bytes = match fs::symlink_metadata(path) {
            Ok(_) => crate::buildcache::apparent_tree_bytes(path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("测量 site 回收边界失败: {}", path.display()))
            }
        };
        Ok(total.saturating_add(bytes))
    })
}

fn add_measured_reclaim_delta(
    worktree: &Path,
    target: &Path,
    bytes_before: u64,
    freed_bytes: &mut u64,
) -> Result<()> {
    let bytes_after = reclaim_boundary_bytes(worktree, target)?;
    *freed_bytes = freed_bytes.saturating_add(bytes_before.saturating_sub(bytes_after));
    Ok(())
}

fn prune_released_registry_batch(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    released: &[Site],
) -> Result<BTreeSet<String>> {
    let registry = gitx::worktree_registry(root)?;
    let prunable = registry
        .iter()
        .filter(|entry| entry.prunable)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    if prunable.len() < 2 {
        return Ok(BTreeSet::new());
    }

    let active = active_sites(events);
    let active_paths = active
        .iter()
        .map(|site| site.worktree.clone())
        .collect::<BTreeSet<_>>();
    let mut eligible = BTreeSet::new();
    let mut complete_prunable = BTreeMap::new();
    for site in released {
        if !site.has_production_path_contract()
            || active_paths.contains(&site.worktree)
            || !complete_journal_paths_are_exclusive(events, site)
        {
            continue;
        }
        let worktree = no_symlink_ancestors(root, &site.worktree)?;
        let _target = no_symlink_ancestors(root, &site.target)?;
        if worktree.exists() {
            continue;
        }
        if let Some(journal) = read_journal(root, round, site)? {
            if journal.phase == "complete" {
                // The global Git prune is an effect boundary too.  Validate
                // the immutable journal, explicit release authority, and the
                // all-generation path predicate before allowing it to run;
                // reap_one must never discover one of these conflicts after
                // the shared registry mutation has already happened.
                if !complete_journal_release_is_explicit(events, round, site) {
                    continue;
                }
                if prunable.contains(&worktree) {
                    complete_prunable.insert(worktree.clone(), site.site_id.clone());
                }
            }
        }
        eligible.insert(worktree);
    }
    if !prunable.is_subset(&eligible) {
        // Git exposes only a global prune operation.  Leave the registry
        // untouched unless every candidate belongs to this exact released set;
        // reap_one will surface the per-site refusal and the invariant stays red.
        return Ok(BTreeSet::new());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "prune", "--expire", "now"])
        .output()
        .context("启动 exact released worktree registry prune 失败")?;
    if !output.status.success() {
        bail!(
            "exact released worktree registry prune 失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let remaining = gitx::worktree_registry(root)?
        .into_iter()
        .filter(|entry| prunable.contains(&entry.path))
        .map(|entry| entry.path)
        .collect::<Vec<_>>();
    if !remaining.is_empty() {
        bail!("released registry batch prune 后仍有注册项: {remaining:?}");
    }
    Ok(complete_prunable.into_values().collect())
}

fn reap_one(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    site: &Site,
    registry_was_pruned: bool,
    outcome: &mut ReapOutcome,
    freed_bytes: &mut u64,
) -> Result<ReapOne> {
    let journal_phase_hint = fs::read(journal_path(root, round, site))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get("phase")?.as_str().map(str::to_string));
    let mut journal = match read_journal(root, round, site) {
        Ok(Some(journal)) => journal,
        Ok(None) => CleanupJournal {
            version: 1,
            round: round.to_string(),
            site: site.clone(),
            phase: "planned".to_string(),
            quarantine: None,
            note: None,
        },
        Err(error) => {
            let reason = format!("site cleanup journal refusal: {error:#}");
            // A parseable complete receipt with identity/version drift is
            // ambiguous ABA evidence, not authority to mutate either physical
            // state or the ledger.  Preserve the historical escalation
            // behaviour for malformed/non-complete first-pass journals.
            return Ok(if journal_phase_hint.as_deref() == Some("complete") {
                ReapOne::quiet_refusal(reason)
            } else {
                ReapOne::refused(reason)
            });
        }
    };
    let complete_replay = journal.phase == "complete";
    let refusal = |reason: String| {
        if complete_replay {
            ReapOne::quiet_refusal(reason)
        } else {
            ReapOne::refused(reason)
        }
    };
    if !site.has_production_path_contract() {
        return Ok(refusal(
            "lease paths 不符合 canonical generation 命名；只报告不删除".to_string(),
        ));
    }
    if site.role == SiteRole::Implement {
        if let Some(active) = active_sites(events)
            .into_iter()
            .find(|active| active.worktree == site.worktree)
        {
            return Ok(refusal(format!(
                "active lease {} shares worktree {}",
                active.site_id, site.worktree
            )));
        }
    }
    let worktree = match no_symlink_ancestors(root, &site.worktree) {
        Ok(worktree) => worktree,
        Err(error) if complete_replay => {
            return Ok(ReapOne::quiet_refusal(format!(
                "complete journal worktree path refusal: {error:#}"
            )))
        }
        Err(error) => return Err(error),
    };
    let target = match no_symlink_ancestors(root, &site.target) {
        Ok(target) => target,
        Err(error) if complete_replay => {
            return Ok(ReapOne::quiet_refusal(format!(
                "complete journal target path refusal: {error:#}"
            )))
        }
        Err(error) => return Err(error),
    };
    if site.role == SiteRole::Implement && worktree.exists() {
        let recorded = crate::reclaim::recorded_tasks_from_events(events);
        let criteria =
            crate::reclaim::observe_task_site_criteria(root, &worktree, &site.task_id, &recorded);
        if let crate::reclaim::TaskSiteDisposition::Keep { reason } =
            crate::reclaim::decide_task_site_reclaim(&criteria)
        {
            return Ok(refusal(format!("task site criteria not met: {reason}")));
        }
    }
    if complete_replay {
        // A `complete` journal is the receipt of a teardown already performed,
        // never a veto on future teardown (COMPLETE_JOURNAL_ABA_CONTRACT_V1).
        // Compare against the canonical registry rendering so a symlinked
        // scratch root cannot disguise a still-registered entry.
        let still_registered = match registered_worktree_snapshot(root) {
            Ok(registry) => registry.paths.iter().any(|path| path == &site.worktree),
            Err(error) => {
                return Ok(ReapOne::quiet_refusal(format!(
                    "complete journal registry probe failed: {error:#}"
                )))
            }
        };
        let physical_reappeared = worktree.exists() || target.exists() || still_registered;
        if !physical_reappeared && !registry_was_pruned {
            return Ok(ReapOne::AlreadyComplete);
        }
        if !complete_journal_release_is_explicit(events, round, site) {
            return Ok(ReapOne::quiet_refusal(
                "complete journal ABA 缺少唯一 canonical WorkspaceReleased/SiteRetired 授权",
            ));
        }
        // ABA: the exact released generation reappeared after its receipt.
        // The explicit release/retirement authority has just been re-folded
        // from fresh ledger bytes inside this same ledger critical section;
        // now refuse any path claimed by a different canonical historical
        // lease.  A later generation that already folded Released is still a
        // distinct owner and cannot disappear from this point-of-use guard.
        if !complete_journal_paths_are_exclusive(events, site) {
            return Ok(ReapOne::quiet_refusal(
                "其他 canonical lease 共享重建现场路径；complete journal 不授权拆除",
            ));
        }
        if !physical_reappeared {
            // The batch preflight proved every exact ABA candidate before the
            // only available global Git prune.  Count that registry-only
            // physical replay exactly once even though reap_one now observes
            // the post-prune registry snapshot.
            return Ok(ReapOne::Removed);
        }
        // The historical journal bytes stay untouched: the replay reuses the
        // physical phase with a frozen journal sink, so the original
        // `complete` receipt is kept on success and every byte survives any
        // refusal.  No ledger event is minted either way.
        let mut sink = JournalSink::frozen(&mut journal);
        return match reap_physical(
            root,
            round,
            site,
            &worktree,
            &target,
            outcome,
            freed_bytes,
            &mut sink,
        ) {
            Ok(result) => Ok(result.without_event()),
            Err(error) => Ok(ReapOne::quiet_refusal(format!(
                "complete journal physical replay failed: {error:#}"
            ))),
        };
    }
    write_journal(root, round, &journal)?;
    let mut sink = JournalSink::live(&mut journal);
    reap_physical(
        root,
        round,
        site,
        &worktree,
        &target,
        outcome,
        freed_bytes,
        &mut sink,
    )
}

/// Write-through for [`CleanupJournal`] phase transitions.  `Live` persists
/// every transition exactly like a first teardown; `Frozen` backs the B314
/// complete-journal ABA replay and drops every write, keeping the historical
/// receipt bytes bit-identical whether the replay reaps, refuses, or fails.
struct JournalSink<'a> {
    journal: &'a mut CleanupJournal,
    frozen: bool,
}

impl<'a> JournalSink<'a> {
    fn live(journal: &'a mut CleanupJournal) -> Self {
        Self {
            journal,
            frozen: false,
        }
    }

    fn frozen(journal: &'a mut CleanupJournal) -> Self {
        Self {
            journal,
            frozen: true,
        }
    }

    fn persist(&self, root: &Path, round: &str) -> Result<()> {
        if self.frozen {
            return Ok(());
        }
        write_journal(root, round, self.journal)
    }
}

fn maintenance_git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    if !output.status.success() {
        bail!(
            "read-only Git observation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

// Shared by the observation path and the physical reaper. It cannot write the index.
fn physical_site_observation(
    root: &Path,
    site: &Site,
    worktree: &Path,
) -> Result<std::result::Result<(Vec<String>, u64), String>> {
    if site.role != SiteRole::Implement
        && (!gitx::same_common_dir(root, worktree)?
            || !gitx::worktree_is_detached(worktree)?
            || gitx::rev_parse(worktree, "HEAD")? != site.reviewed_head)
    {
        return Ok(Err(
            "worktree common-dir/detached HEAD 与 lease 不一致".into()
        ));
    }
    if !maintenance_git(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=no"],
    )?
    .is_empty()
    {
        return Ok(Err("现场含 tracked/staged 修改；拒绝自动拆除".into()));
    }
    Ok(Ok(untracked_inventory(worktree)?))
}

/// Physical teardown phase shared by a first teardown and the B314
/// complete-journal ABA replay.  The caller has already applied canonical
/// paths, ancestor no-symlink, and implement-role task-site criteria.  This
/// phase applies the review-role common-dir/detached/reviewed-HEAD checks plus
/// the shared tracked/staged cleanliness, bounded untracked quarantine, and
/// exact-path target/registry removal.  The two callers differ only in the
/// [`JournalSink`]; the ABA replay never rewrites the historical receipt.
#[allow(clippy::too_many_arguments)]
fn reap_physical(
    root: &Path,
    round: &str,
    site: &Site,
    worktree: &Path,
    target: &Path,
    outcome: &mut ReapOutcome,
    freed_bytes: &mut u64,
    sink: &mut JournalSink<'_>,
) -> Result<ReapOne> {
    let bytes_before = reclaim_boundary_bytes(worktree, target)?;

    if worktree.exists() {
        let (untracked, bytes) = match physical_site_observation(root, site, worktree)? {
            Ok(observation) => observation,
            Err(reason) => return Ok(ReapOne::refused(reason)),
        };
        let untracked_refs = untracked.iter().map(String::as_str).collect::<Vec<_>>();
        match ResidueDisposition::for_untracked(&untracked_refs, bytes) {
            ResidueDisposition::Proceed => {}
            ResidueDisposition::Quarantine { .. } => {
                let quarantine = quarantine_entries(root, round, site, worktree, &untracked)?;
                sink.journal.phase = "quarantined".to_string();
                sink.journal.quarantine = Some(quarantine.display().to_string());
                sink.persist(root, round)?;
                outcome.quarantined.push(site.site_id.clone());
            }
            ResidueDisposition::RefuseAndEscalate { bytes, limit } => {
                return Ok(ReapOne::refused(format!(
                    "untracked residue {bytes} bytes 超过 quarantine 上限 {limit}"
                )))
            }
        }
        if let Err(error) = gitx::worktree_remove(root, worktree) {
            add_measured_reclaim_delta(worktree, target, bytes_before, freed_bytes)?;
            sink.journal.note = Some(format!("worktree remove failed: {error:#}"));
            sink.persist(root, round)?;
            return Ok(ReapOne::refused(
                "worktree remove 失败；按契约不触碰 target",
            ));
        }
    }
    if let Err(error) = gitx::worktree_prune_exact(root, worktree) {
        add_measured_reclaim_delta(worktree, target, bytes_before, freed_bytes)?;
        return Err(error);
    }
    sink.journal.phase = "worktree-removed".to_string();
    if let Err(error) = sink.persist(root, round) {
        add_measured_reclaim_delta(worktree, target, bytes_before, freed_bytes)?;
        return Err(error);
    }

    let target_failure = if target.exists() {
        match crate::util::remove_dir_all_with_enotempty_retry(target) {
            Ok(()) => None,
            Err(error) => Some(format!("target remove failed: {error}")),
        }
    } else {
        None
    };
    if let Some(note) = target_failure {
        add_measured_reclaim_delta(worktree, target, bytes_before, freed_bytes)?;
        sink.journal.phase = "target-remove-failed".to_string();
        sink.journal.note = Some(note.clone());
        sink.persist(root, round)?;
        return Ok(ReapOne::target_failed(note));
    }
    sink.journal.phase = "complete".to_string();
    sink.journal.note = None;
    if let Err(error) = sink.persist(root, round) {
        add_measured_reclaim_delta(worktree, target, bytes_before, freed_bytes)?;
        return Err(error);
    }
    add_measured_reclaim_delta(worktree, target, bytes_before, freed_bytes)?;
    Ok(ReapOne::Removed)
}

fn looks_like_legacy_review_site(path: &str) -> bool {
    let Some(name) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with("review-")
        || name.contains("-primary-")
        || name.contains("-secondary-")
        || name.contains("-nongate-")
}

struct RegistrySnapshot {
    paths: Vec<String>,
    prunable: Vec<String>,
}

fn registered_worktree_snapshot(root: &Path) -> Result<RegistrySnapshot> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("canonicalize site registry root 失败: {}", root.display()))?;
    let mut paths = Vec::new();
    let mut prunable = Vec::new();
    for entry in gitx::worktree_registry(root)? {
        let normalized = if entry.path.exists() {
            fs::canonicalize(&entry.path).with_context(|| {
                format!(
                    "canonicalize registered worktree 失败: {}",
                    entry.path.display()
                )
            })?
        } else {
            if entry.path.is_absolute() {
                entry.path.clone()
            } else {
                canonical_root.join(&entry.path)
            }
        };
        let rendered = if normalized == canonical_root {
            ".".to_string()
        } else if let Ok(relative) = normalized.strip_prefix(&canonical_root) {
            relative
                .to_str()
                .context("registered worktree relative path 不是 UTF-8")?
                .to_string()
        } else {
            normalized
                .to_str()
                .context("registered external worktree path 不是 UTF-8")?
                .to_string()
        };
        if entry.prunable {
            prunable.push(rendered.clone());
        }
        paths.push(rendered);
    }
    paths.sort();
    paths.dedup();
    prunable.sort();
    prunable.dedup();
    Ok(RegistrySnapshot { paths, prunable })
}

fn refusal_event(round: &str, site: &Site, reason: &str) -> EventRecord {
    ledger::event(
        "EscalationRaised",
        "runtime:orch",
        Some(&site.task_id),
        Some(round),
        serde_json::json!({
            "stage": "site-cleanup-storage",
            "siteId": site.site_id,
            "generation": site.generation,
            "attemptId": site.attempt_id,
            "role": site.role.as_str(),
            "agent": site.agent,
            "reason": reason,
            "action": "refuse-delete",
        }),
    )
}

fn refusal_is_recorded(events: &[EventRecord], site: &Site, reason: &str) -> bool {
    events.iter().any(|event| {
        event.kind == "EscalationRaised"
            && event.actor == "runtime:orch"
            && event.task_id.as_deref() == Some(site.task_id.as_str())
            && payload_string(event, "stage") == Some("site-cleanup-storage")
            && payload_string(event, "siteId") == Some(site.site_id.as_str())
            && payload_u32(event, "generation") == Some(site.generation)
            && payload_string(event, "reason") == Some(reason)
    })
}

/// Reap every released generation.  The fresh fold and every destructive
/// operation run inside the same ledger critical section as site provisioning.
/// Replaying an immutable complete journal is narrower than ordinary release
/// folding: it requires one canonical `WorkspaceReleased` or anchored
/// `SiteRetired`, rechecks every physical guard, and never appends a refusal or
/// completion event.
pub fn reap_released_sites(root: &Path, round: &str) -> Result<ReapOutcome> {
    // Keep reconciliation at foreground callers.  Managed termination
    // reconciliation already flows reconcile -> reap; calling it back from
    // here would create a cycle and invert the single ledger-lock direction.
    reap_released_sites_with_hook(root, round, || Ok(()))
}

/// Compact progress report for foreground cleanup commands.  A missing root
/// is an empty reclamation domain, not an error that discards an empty report.
pub fn reap_released_sites_reported(root: &Path, round: &str) -> Result<SiteReapReport> {
    if matches!(
        fs::symlink_metadata(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return Ok(SiteReapReport::default());
    }
    let outcome = reap_released_sites(root, round)?;
    Ok(SiteReapReport {
        reaped: outcome.removed,
        refused: outcome.refused_details,
        freed_bytes: outcome.freed_bytes,
    })
}

fn reap_released_sites_with_hook<F>(root: &Path, round: &str, before_reap: F) -> Result<ReapOutcome>
where
    F: FnOnce() -> Result<()>,
{
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let initial = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取 site GC 账本失败: {}", ledger_path.display()))?;
    if !initial.bad_lines.is_empty() {
        bail!("site GC 拒绝坏账本：活跃判据 fail-closed，不触碰任何现场");
    }

    let mut outcome = ReapOutcome::default();
    ledger::append_checked(root, round, |events| {
        before_reap()?;
        let mut escalations = Vec::new();
        let released = reclaimable_sites(events);
        let batch_pruned_complete = prune_released_registry_batch(root, round, events, &released)?;
        for site in released {
            // A second fold at the point of use makes the destructive arm
            // explicit.  A newly appended generation cannot enter this locked
            // region until the current decision and deletion have completed.
            if !matches!(
                LeaseState::of(events, &site.identity(), site.generation),
                LeaseState::Released { .. }
            ) {
                continue;
            }
            let mut freed_bytes = 0;
            let result = reap_one(
                root,
                round,
                events,
                &site,
                batch_pruned_complete.contains(&site.site_id),
                &mut outcome,
                &mut freed_bytes,
            );
            outcome.freed_bytes = outcome.freed_bytes.saturating_add(freed_bytes);
            match result {
                Ok(ReapOne::Removed) => outcome.removed.push(site.site_id.clone()),
                Ok(ReapOne::AlreadyComplete) => {}
                Ok(ReapOne::Refused {
                    reason,
                    append_event,
                }) => {
                    outcome.refused.push(format!("{}: {reason}", site.site_id));
                    outcome
                        .refused_details
                        .push((site.site_id.clone(), reason.clone()));
                    if append_event && !refusal_is_recorded(events, &site, &reason) {
                        escalations.push(refusal_event(round, &site, &reason));
                    }
                }
                Ok(ReapOne::TargetFailed {
                    reason,
                    append_event,
                }) => {
                    outcome
                        .target_failures
                        .push(format!("{}: {reason}", site.site_id));
                    outcome
                        .refused_details
                        .push((site.site_id.clone(), reason.clone()));
                    if append_event && !refusal_is_recorded(events, &site, &reason) {
                        escalations.push(refusal_event(round, &site, &reason));
                    }
                }
                Err(error) => {
                    let reason = format!("site cleanup failed: {error:#}");
                    outcome.refused.push(format!("{}: {reason}", site.site_id));
                    outcome
                        .refused_details
                        .push((site.site_id.clone(), reason.clone()));
                    if !refusal_is_recorded(events, &site, &reason) {
                        escalations.push(refusal_event(round, &site, &reason));
                    }
                }
            }
        }

        let all_lease_paths = events
            .iter()
            .filter(|event| event.kind == "WorkspaceLeased")
            .filter_map(|event| parse_lease(event).ok())
            .map(|site| site.worktree)
            .collect::<BTreeSet<_>>();
        let registry = registered_worktree_snapshot(root)?;
        outcome.legacy_reports = registry
            .paths
            .iter()
            .filter(|path| looks_like_legacy_review_site(path) && !all_lease_paths.contains(*path))
            .map(|path| LegacySiteReport {
                worktree: path.clone(),
                reason: "无可验证 WorkspaceLeased 映射；discovery-only，未删除".to_string(),
            })
            .collect();

        let active = active_sites(events);
        let invariant_active = active
            .iter()
            .filter(|site| {
                site.role != SiteRole::Implement || registry.paths.contains(&site.worktree)
            })
            .cloned()
            .collect::<Vec<_>>();
        let active_paths = invariant_active
            .iter()
            .map(|site| site.worktree.clone())
            .collect::<BTreeSet<_>>();
        // Every registered non-active worktree is an explicit preserve: the
        // primary checkout, implementation sites, refused released sites, and
        // discovery-only legacy names.  Nothing disappears from the equation
        // merely because its name is unfamiliar.
        let preserves = registry
            .paths
            .iter()
            .filter(|path| !active_paths.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        outcome.registry_invariant = Some(worktree_registry_invariant_with_prunable(
            &registry.paths,
            &invariant_active,
            &preserves,
            &registry.prunable,
        ));
        Ok(escalations)
    })?;
    Ok(outcome)
}

pub(crate) fn maintenance_sites(events: &[EventRecord]) -> (Vec<Site>, Option<String>) {
    let mut sites = BTreeMap::new();
    let mut errors = Vec::new();
    for event in events.iter().filter(|e| e.kind == "WorkspaceLeased") {
        match parse_lease(event) {
            Ok(site) => {
                sites.insert(site.site_id.clone(), site);
            }
            Err(error) => errors.push(error),
        }
    }
    if !site_evidence_is_unambiguous(events) {
        errors.push("ambiguous workspace lifecycle evidence".into());
    }
    (
        sites.into_values().collect(),
        if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
    )
}

fn maintenance_evidence(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    site: &Site,
) -> Result<Option<String>> {
    use sha2::Digest;
    let lease = events
        .iter()
        .find(|e| {
            e.kind == "WorkspaceLeased"
                && payload_string(e, "siteId") == Some(site.site_id.as_str())
        })
        .context("missing exact lease")?;
    if lease_is_pre_spawn_rejected(events, lease) {
        return Ok(None);
    }
    if site.role == SiteRole::Implement {
        let recorded = events
            .iter()
            .rposition(|e| {
                e.kind == "TaskRecorded"
                    && e.actor == "runtime:orch"
                    && e.task_id.as_deref() == Some(site.task_id.as_str())
            })
            .context("task has no trusted Recorded fact")?;
        let head = events[..recorded]
            .iter()
            .rev()
            .find(|e| {
                e.kind == "MergeStarted" && e.task_id.as_deref() == Some(site.task_id.as_str())
            })
            .and_then(|e| payload_string(e, "headSha"))
            .filter(|s| full_sha(s))
            .context("recorded task lacks fixed merged candidate")?;
        let report = format!(
            "coordination/rounds/{round}/reports/{}-REPORT.md",
            site.task_id
        );
        let path = no_symlink_ancestors(root, &report)?;
        let bytes = fs::read(path).context("recorded report is not preserved")?;
        let committed = maintenance_git(root, &["show", &format!("{head}:{report}")])?;
        if bytes.is_empty() || bytes != committed {
            bail!("preserved task report differs from the fixed candidate");
        }
        return Ok(Some(head.to_string()));
    }
    let terminals = events
        .iter()
        .filter(|event| managed_termination_matches(event, site))
        .collect::<Vec<_>>();
    if terminals.len() != 1 {
        bail!("no unique native terminal for released site");
    }
    let terminal = terminals[0];
    let payload = terminal
        .payload
        .as_ref()
        .context("native terminal payload absent")?;
    if payload.get("turnEnded").and_then(|v| v.as_bool()) != Some(true)
        || payload_string(terminal, "state") != Some("answered")
        || payload
            .get("mechanicalTerminalAbsent")
            .and_then(|v| v.as_bool())
            != Some(false)
        || payload
            .pointer("/channelBinding/fixedHead")
            .and_then(|v| v.as_str())
            != Some(site.reviewed_head.as_str())
    {
        bail!("native end or fixed HEAD is unknown; client exit is not authority");
    }
    let delivered = events
        .iter()
        .filter(|e| {
            e.kind == "ReviewDelivered"
                && e.actor == "runtime:orch"
                && e.task_id.as_deref() == Some(site.task_id.as_str())
                && payload_string(e, "attemptId") == Some(site.attempt_id.as_str())
                && payload_string(e, "agent") == Some(site.agent.as_str())
                && payload_string(e, "wakeId") == site.wake_id.as_deref()
                && payload_string(e, "reviewedHead") == Some(site.reviewed_head.as_str())
        })
        .collect::<Vec<_>>();
    if delivered.len() > 1 {
        bail!("ambiguous preserved review binding");
    }
    let (path, expected_hash, expected_bytes) = if let Some(delivered) = delivered.first() {
        if payload_string(delivered, "terminalEventId") != Some(terminal.event_id.as_str()) {
            bail!("review terminal binding differs");
        }
        let relative = payload_string(delivered, "path").context("review artifact path absent")?;
        if !Path::new(relative).starts_with(format!("coordination/rounds/{round}/reviews")) {
            bail!("review artifact is outside preserved round");
        }
        (
            no_symlink_ancestors(root, relative)?,
            payload_string(delivered, "sha256").context("review digest absent")?,
            delivered
                .payload
                .as_ref()
                .and_then(|p| p.get("bytes"))
                .and_then(|v| v.as_u64()),
        )
    } else {
        let output = Path::new(
            payload_string(terminal, "outputPath").context("native output is not preserved")?,
        );
        let relative = if output.is_absolute() {
            output
                .strip_prefix(root)
                .context("native output outside project")?
        } else {
            output
        };
        if !relative.starts_with(format!("coordination/runtime/review-inbox/{round}")) {
            bail!("native output outside preserved review inbox");
        }
        (
            no_symlink_ancestors(
                root,
                relative.to_str().context("non-UTF8 native output path")?,
            )?,
            payload_string(terminal, "outputSha256").context("native output digest absent")?,
            None,
        )
    };
    if path.starts_with(root.join(&site.worktree)) || path.starts_with(root.join(&site.target)) {
        bail!("evidence is inside a disposable site");
    }
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 16 * 1024 * 1024 {
        bail!("review evidence is not a bounded regular file");
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty()
        || expected_hash != hex::encode(sha2::Sha256::digest(&bytes))
        || expected_bytes.is_some_and(|n| n != bytes.len() as u64)
    {
        bail!("preserved review bytes/hash differ from native delivery");
    }
    Ok(Some(site.reviewed_head.clone()))
}

fn maintenance_quiet(paths: &[PathBuf]) -> Result<()> {
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
        .context("lsof unavailable; active use cannot be ruled out")?;
    for path in crate::reclaim::boundary_union(paths) {
        if !path.exists() {
            continue;
        }
        let output = Command::new(lsof)
            .args(["-nP", "-t", "+D"])
            .arg(&path)
            .output()?;
        if output.status.code() != Some(1) || !output.stdout.is_empty() || !output.stderr.is_empty()
        {
            bail!(
                "open files or uncertain use at {}; no signals sent",
                path.display()
            );
        }
    }
    Ok(())
}

// A bounded observation of a native Git monitor, never a lifecycle/release receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeMonitor {
    pid: u32,
    process: String,
    worktree: PathBuf,
    admin: PathBuf,
    ipc: PathBuf,
    identities: Vec<(u64, u64)>,
    proof_mode: String,
}

fn monitor_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    for p in path.ancestors() {
        if fs::symlink_metadata(p)?.file_type().is_symlink() { bail!("monitor path contains symlink: {}", p.display()); }
    }
    let m = fs::metadata(path)?;
    Ok((m.dev(), m.ino()))
}

fn monitor_pids(output: &std::process::Output) -> Result<std::collections::BTreeSet<u32>> {
    if !matches!(output.status.code(), Some(0 | 1)) || !output.stderr.is_empty() {
        bail!("uncertain monitor opener observation");
    }
    let text = std::str::from_utf8(&output.stdout)?;
    let mut pids = std::collections::BTreeSet::new();
    for line in text.lines() {
        if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) { bail!("invalid opener PID"); }
        let pid: u32 = line.parse()?;
        if pid == 0 { bail!("invalid zero PID"); }
        pids.insert(pid);
    }
    if pids.is_empty() && output.status.code() != Some(1) { bail!("ambiguous empty opener result"); }
    Ok(pids)
}

// Keep direct IPC attribution separate from the relative-listener fallback proof.
fn monitor_endpoint_owner(output: &std::process::Output, boundary_pids: &BTreeSet<u32>) -> Result<bool> {
    let endpoint_pids = monitor_pids(output)?;
    if !endpoint_pids.is_empty() && endpoint_pids != *boundary_pids {
        bail!("conflicting native IPC owner");
    }
    Ok(output.status.code() == Some(0) && endpoint_pids == *boundary_pids)
}

fn native_monitor_command(worktree: &Path, verb: &str) -> Result<std::process::Output> {
    Ok(Command::new("git").args(["--no-optional-locks", "fsmonitor--daemon", verb])
        .current_dir(worktree).env("LC_ALL", "C").env("GIT_OPTIONAL_LOCKS", "0").output()?)
}

fn monitor_status(worktree: &Path, watching: bool) -> Result<()> {
    let o = native_monitor_command(worktree, "status")?;
    let expected = format!("fsmonitor-daemon is {}watching '{}'", if watching { "" } else { "not " }, worktree.display());
    if o.status.code() != Some(if watching { 0 } else { 1 }) || !o.stderr.is_empty()
        || std::str::from_utf8(&o.stdout)?.trim() != expected { bail!("native monitor status does not bind exact worktree"); }
    Ok(())
}

fn monitor_fields(bytes: &[u8]) -> Result<Vec<std::collections::BTreeMap<char, String>>> {
    let mut records = Vec::new();
    let mut record = std::collections::BTreeMap::new();
    for field in std::str::from_utf8(bytes)?.split('\0') {
        let field = field.trim_start_matches('\n');
        if field.is_empty() { continue; }
        let key = field.chars().next().unwrap();
        if matches!(key, 'p' | 'f') && !record.is_empty() { records.push(record); record = std::collections::BTreeMap::new(); }
        if record.insert(key, field[1..].to_owned()).is_some() { bail!("duplicate monitor field"); }
    }
    if !record.is_empty() { records.push(record); }
    Ok(records)
}

fn prove_native_monitor(worktree: &Path, target: &Path) -> Result<NativeMonitor> {
    use std::os::unix::fs::FileTypeExt;
    if !cfg!(target_os = "macos") { bail!("native monitor proof unsupported on this platform"); }
    let lsof = "/usr/sbin/lsof";
    let mut boundary_pids = std::collections::BTreeSet::new();
    for p in crate::reclaim::boundary_union(&[worktree.to_path_buf(), target.to_path_buf()]) {
        if p.exists() { boundary_pids.extend(monitor_pids(&Command::new(lsof).args(["-nP", "-t", "+D"]).arg(p).output()?)?); }
    }
    if boundary_pids.len() != 1 { bail!("monitor is not the unique boundary opener"); }
    let pid = *boundary_pids.iter().next().unwrap();
    let direct = Command::new(lsof).args(["-nP", "-t", "--"]).arg(worktree).output()?;
    if direct.status.code() != Some(0) || monitor_pids(&direct)? != boundary_pids { bail!("monitor does not own exact worktree directory"); }
    let path_from_git = |args: &[&str]| -> Result<PathBuf> {
        let raw = String::from_utf8(maintenance_git(worktree, args)?)?;
        let p = PathBuf::from(raw.trim());
        let p = if p.is_absolute() { p } else { worktree.join(p) };
        // Git may return common-dir with '..'; resolve it before validating each ancestor.
        monitor_identity(&p)?;
        let p = fs::canonicalize(p)?; monitor_identity(&p)?; Ok(p)
    };
    let admin = path_from_git(&["rev-parse", "--absolute-git-dir"])?;
    let common = path_from_git(&["rev-parse", "--git-common-dir"])?;
    if admin.parent() != Some(common.join("worktrees").as_path()) || admin == common { bail!("monitor admin is not a private linked-worktree directory"); }
    let ipc = admin.join("fsmonitor--daemon.ipc");
    let ipc_id = monitor_identity(&ipc)?;
    if !fs::symlink_metadata(&ipc)?.file_type().is_socket() { bail!("native IPC is not a socket"); }
    let endpoint = Command::new(lsof).args(["-nP", "-t", "--"]).arg(&ipc).output()?;
    let direct_endpoint = monitor_endpoint_owner(&endpoint, &boundary_pids)?;
    let process_out = Command::new("/bin/ps").args(["-p", &pid.to_string(), "-o", "uid=", "-o", "lstart=", "-o", "command="]).env("LC_ALL", "C").output()?;
    if !process_out.status.success() || !process_out.stderr.is_empty() { bail!("monitor process identity unavailable"); }
    let process = String::from_utf8(process_out.stdout)?.trim().to_owned();
    let words: Vec<_> = process.split_whitespace().collect();
    let uid = Command::new("/usr/bin/id").arg("-u").output()?;
    if !uid.status.success() || !uid.stderr.is_empty() || words.len() < 10
        || words[0] != std::str::from_utf8(&uid.stdout)?.trim()
        || Path::new(words[6]).file_name().and_then(|s|s.to_str()) != Some("git")
        || words[7..9] != ["fsmonitor--daemon", "run"]
        || !words[9..].contains(&"--detach")
        || words[9..].iter().any(|w| *w != "--detach" && !w.strip_prefix("--ipc-threads=").is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))) {
        bail!("process is not the current user's native Git monitor run");
    }
    let exec = PathBuf::from(String::from_utf8(maintenance_git(worktree, &["--exec-path"])?)?.trim());
    let git_image = exec.join("git");
    let dispatcher = exec.parent().and_then(Path::parent).context("invalid Git exec path")?.join("bin/git");
    let images = [monitor_identity(&fs::canonicalize(&git_image)?)?, monitor_identity(&fs::canonicalize(&dispatcher)?)?];
    let mut dirs = std::collections::BTreeSet::new();
    for p in [worktree, admin.as_path()] {
        dirs.extend(p.ancestors().map(Path::to_path_buf));
        let physical = PathBuf::from(format!("/System/Volumes/Data{}", p.display()));
        if monitor_identity(&physical).ok() == monitor_identity(p).ok() {
            dirs.extend(physical.ancestors().map(Path::to_path_buf));
        }
    }
    let output = Command::new(lsof).args(["-nP", "-a", "-p", &pid.to_string(), "-F0pcuftDin"]).output()?;
    if !output.status.success() || !output.stderr.is_empty() { bail!("complete monitor descriptors unavailable"); }
    validate_monitor_descriptors(&output.stdout, pid, words[0], worktree, &admin, &ipc, &images, &dirs, direct_endpoint)?;
    monitor_status(worktree, true)?;
    Ok(NativeMonitor { pid, process, worktree: worktree.to_path_buf(), admin: admin.clone(), ipc,
        identities: vec![monitor_identity(worktree)?, monitor_identity(&admin)?, ipc_id, images[0], images[1]],
        proof_mode: if direct_endpoint { "direct-ipc-owner" } else { "relative-listener-native-admin-conjunction" }.into() })
}

fn validate_monitor_descriptors(
    bytes: &[u8], pid: u32, uid: &str, worktree: &Path, admin: &Path, ipc: &Path,
    images: &[(u64, u64)], dirs: &BTreeSet<PathBuf>, direct_endpoint: bool,
) -> Result<()> {
    let records = monitor_fields(bytes)?;
    let header = records.first().context("missing monitor header")?;
    if header.get(&'p') != Some(&pid.to_string()) || header.get(&'u').map(String::as_str) != Some(uid) { bail!("descriptor process identity mismatch"); }
    let wt_id = monitor_identity(worktree)?; let admin_id = monitor_identity(&admin)?;
    let mut have_wt = false; let mut have_admin = false; let mut have_image = false; let mut listeners = 0;
    let mut relative_listener = false;
    for r in records.iter().skip(1) {
        let fd = r.get(&'f').context("missing descriptor")?;
        let ty = r.get(&'t').context("missing descriptor type")?;
        let name = r.get(&'n').map(String::as_str).unwrap_or("");
        match ty.as_str() {
            "DIR" => {
                let p = Path::new(name);
                if !dirs.contains(p) { bail!("foreign monitor directory {name}"); }
                let id = monitor_identity(p)?;
                let device = r.get(&'D').and_then(|s|s.strip_prefix("0x")).context("directory device missing")?;
                let inode: u64 = r.get(&'i').context("directory inode missing")?.parse()?;
                if (u64::from_str_radix(device,16)?, inode) != id { bail!("monitor directory identity changed"); }
                have_wt |= id == wt_id; have_admin |= id == admin_id;
            }
            "REG" if fd == "txt" => {
                let id = monitor_identity(Path::new(name))?;
                let device = r.get(&'D').and_then(|s| s.strip_prefix("0x")).context("image device missing")?;
                let inode: u64 = r.get(&'i').context("image inode missing")?.parse()?;
                if (u64::from_str_radix(device, 16)?, inode) != id { bail!("mapped monitor image changed"); }
                if images.contains(&id) { have_image = true; }
                else if name != "/usr/lib/dyld" { bail!("foreign monitor executable image"); }
            }
            "CHR" if matches!(fd.as_str(), "0" | "1" | "2") && name == "/dev/null" => {}
            "KQUEUE" => {}
            "unix" if name.starts_with("->0x") && name[4..].bytes().all(|b|b.is_ascii_hexdigit()) => {}
            "unix" => {
                if name == ipc.to_string_lossy() { listeners += 1; }
                else if name == "fsmonitor--daemon.ipc" { listeners += 1; relative_listener = true; }
                else { bail!("foreign named monitor listener"); }
            }
            _ => bail!("unexpected monitor descriptor {fd}/{ty}/{name}"),
        }
    }
    if !have_wt || !have_admin || !have_image || listeners != 1 || (!direct_endpoint && !relative_listener) {
        bail!("incomplete dedicated monitor identity bundle");
    }
    Ok(())
}


#[cfg(all(test, target_os = "macos"))]
mod native_monitor_tests {
    use super::*;
    fn g(root: &Path, args: &[&str]) -> String {
        let o=Command::new("git").args(["--no-optional-locks","-c","core.fsmonitor=false","-c","user.name=fixture","-c","user.email=fixture@example.invalid"]).args(args).current_dir(root).output().unwrap();
        assert!(o.status.success(),"{}",String::from_utf8_lossy(&o.stderr)); String::from_utf8(o.stdout).unwrap().trim().into()
    }
    fn write_maintenance_events(root: &Path, round: &str, es: &[EventRecord]) {
        let bytes=es.iter().map(|e|serde_json::to_string(e).unwrap()+"\n").collect::<String>();
        for rel in [format!("coordination/rounds/{round}/events.jsonl"),format!("coordination/runtime/ledger-wal/{round}.jsonl")] {
            let p=root.join(rel);fs::create_dir_all(p.parent().unwrap()).unwrap();fs::write(p,&bytes).unwrap();
        }
    }
    struct Owned(PathBuf);
    impl Owned { fn start(wt: PathBuf)->Self { let o=Command::new("git").args(["--no-optional-locks","-c","core.fsmonitor=true","fsmonitor--daemon","start"]).current_dir(&wt).output().unwrap();assert!(o.status.success());Self(wt) } }
    impl Drop for Owned { fn drop(&mut self) { if self.0.exists() {let _=native_monitor_command(&self.0,"stop");} } }
    fn fixture(tag: &str) -> (PathBuf, crate::sites::Site, Vec<EventRecord>) {
        use sha2::Digest;
        let root = crate::util::test_scratch_dir(tag);
        g(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("tracked.txt"), b"source\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\norch/target/\ncoordination/\n",
        )
        .unwrap();
        g(&root, &["add", "."]);
        g(&root, &["commit", "-qm", "fixture"]);
        let head = g(&root, &["rev-parse", "HEAD"]);
        let site = crate::sites::Site {
            site_id: "M1-review-probe-g01".into(),
            generation: 1,
            task_id: "M1".into(),
            attempt_id: "M1-A0001".into(),
            role: crate::sites::SiteRole::Review,
            agent: "probe".into(),
            reviewed_head: head.clone(),
            worktree: ".worktrees/review-M1-A0001-review-probe-g01".into(),
            target: "orch/target/review-M1-A0001-review-probe-g01".into(),
            wake_id: Some("wake-M1".into()),
        };
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        g(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                root.join(&site.worktree).to_str().unwrap(),
                &head,
            ],
        );
        fs::create_dir_all(root.join(&site.target)).unwrap();
        fs::write(root.join(&site.target).join("cache"), b"owned-cache").unwrap();
        let evidence = root.join("coordination/runtime/review-inbox/rMaint/M1.md");
        fs::create_dir_all(evidence.parent().unwrap()).unwrap();
        fs::write(&evidence, b"preserved native answer").unwrap();
        let event = |kind: &str, payload| {
            crate::ledger::event(kind, "runtime:orch", Some("M1"), Some("rMaint"), payload)
        };
        let lease = event(
            "WorkspaceLeased",
            serde_json::json!({"siteId":site.site_id,"generation":1,
            "attemptId":site.attempt_id,"role":"review","agent":"probe","reviewedHead":head,
            "wakeId":"wake-M1","paths":{"worktree":site.worktree,"target":site.target}}),
        );
        let terminal = event(
            "ManagedWakeTerminated",
            serde_json::json!({"wakeId":"wake-M1","agent":"probe",
            "managedScopeTerminated":true,"turnEnded":true,"state":"answered","mechanicalTerminalAbsent":false,
            "channelBinding":{"fixedHead":head},"outputPath":evidence,
            "outputSha256":hex::encode(sha2::Sha256::digest(b"preserved native answer"))}),
        );
        let release = event(
            "WorkspaceReleased",
            serde_json::json!({"siteId":site.site_id,"generation":1,
            "attemptId":site.attempt_id,"role":"review","agent":"probe","wakeId":"wake-M1",
            "completionReceipt":crate::sites::MANAGED_COMPLETION_RECEIPT,"terminationEventId":terminal.event_id}),
        );
        let closed = crate::ledger::event(
            "RoundClosed",
            "runtime:orch",
            None,
            Some("rMaint"),
            serde_json::json!({"forced":false}),
        );
        let events = vec![lease, terminal, release, closed];
        write_maintenance_events(&root, "rMaint", &events);
        (root, site, events)
    }


    #[test]
    fn monitor_stop_failure_and_later_refusal_preserve_effect_and_source() {
        use std::cell::Cell;
        for after_stop in [false,true] {
            let (root,site,events)=fixture("b345-stop-failure");let wt=root.join(&site.worktree);let _m=Owned::start(wt.clone());let calls=Cell::new(0);
            let guard=|| {calls.set(calls.get()+1);if after_stop && calls.get()==2 {bail!("injected fresh evidence refusal");}Ok(())};
            let stop=|p:&Path| {if after_stop {native_monitor_command(p,"stop")}else{Ok(Command::new("/usr/bin/false").output()?)}};
            let result=maintain_one_site_guarded(&root,"rMaint",&events,&site,false,&guard,&stop);
            assert_eq!(result.disposition,"failed","{result:#?}");assert_eq!(result.removed_logical_bytes,0);assert!(wt.exists());
            assert!(result.reason.contains(if after_stop {"stopped; IPC absent"}else{"stop attempted"}),"{result:#?}");
            monitor_status(&wt,!after_stop).unwrap();
            if after_stop {let retry=maintain_one_site(&root,"rMaint",&events,&site,false);assert_eq!(retry.disposition,"removed","{retry:#?}");}
        }
    }
    #[test]
    fn monitor_fresh_guard_and_changed_process_precede_stop() {
        use std::cell::Cell;
        for replace in [false,true] {
            let (root,site,events)=fixture("b345-before-stop");let wt=root.join(&site.worktree);let _m=Owned::start(wt.clone());let called=Cell::new(0);let replacement=std::cell::RefCell::new(None);
            let guard=|| {if replace {native_monitor_command(&wt,"stop")?;*replacement.borrow_mut()=Some(Owned::start(wt.clone()));Ok(())}else{bail!("new all-owner refusal")}};
            let stop=|p:&Path| {called.set(called.get()+1);native_monitor_command(p,"stop")};
            let result=maintain_one_site_guarded(&root,"rMaint",&events,&site,false,&guard,&stop);
            assert_eq!(called.get(),0,"stop preceded fresh proof: {result:#?}");assert!(wt.exists());assert_eq!(result.removed_logical_bytes,0);monitor_status(&wt,true).unwrap();
        }
    }
    #[test]
    fn monitor_descriptor_bundle_rejects_missing_admin_extra_listener_and_foreign_identity() {
        let (root,site,_)=fixture("b345-descriptors");let wt=root.join(&site.worktree);let _m=Owned::start(wt.clone());
        let proof=prove_native_monitor(&wt,&root.join(&site.target)).unwrap();
        let raw=Command::new("/usr/sbin/lsof").args(["-nP","-a","-p",&proof.pid.to_string(),"-F0pcuftDin"]).output().unwrap().stdout;
        let text=String::from_utf8(raw.clone()).unwrap();let uid=proof.process.split_whitespace().next().unwrap();let images=&proof.identities[3..];let mut dirs=BTreeSet::new();
        for p in [&wt,&proof.admin] {dirs.extend(p.ancestors().map(Path::to_path_buf));let physical=PathBuf::from(format!("/System/Volumes/Data{}",p.display()));if monitor_identity(&physical).ok()==monitor_identity(p).ok(){dirs.extend(physical.ancestors().map(Path::to_path_buf));}}
        let check=|b:&[u8]| validate_monitor_descriptors(b,proof.pid,uid,&wt,&proof.admin,&proof.ipc,images,&dirs,false);
        check(&raw).unwrap();
        let without_admin=text.split_inclusive('\n').filter(|line| !line.contains(&format!("n{}\0",proof.admin.display()))).collect::<String>();
        let variants=vec![
            without_admin,
            text.replacen(&format!("u{uid}\0"),"u0\0",1),
            text.replacen(&format!("p{}\0",proof.pid),"p0\0",1),
            text.replace(&format!("n{}\0",proof.admin.display()),&format!("n{}\0",root.display())),
            format!("{text}f999\0tunix\0nfsmonitor--daemon.ipc\0\n"),
            text.replace("nfsmonitor--daemon.ipc\0","nforeign.ipc\0"),
            format!("{text}f998\0tREG\0n{}\0\n",root.join("tracked.txt").display()),
            text.replace("/Library/Developer/CommandLineTools/usr/bin/git","/usr/lib/dyld"),
        ];
        for (n,v) in variants.iter().enumerate(){assert!(check(v.as_bytes()).is_err(),"accepted invalid proof {n}");}
    }
    #[test]
    fn monitor_pending_target_sweep_keeps_cache_until_quiet() {
        let (root,site,events)=fixture("b345-ttl");let wt=root.join(&site.worktree);let _m=Owned::start(wt.clone());
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"),"rMaint\n").unwrap();
        let preview=maintain_one_site(&root,"rMaint",&events,&site,true);assert!(preview.eligible,"{preview:#?}");
        let report=crate::buildcache::sweep_targets_for_round(&root,std::time::Duration::ZERO).unwrap();
        assert!(root.join(&site.target).exists(),"pending target swept: {report:#?}");monitor_status(&wt,true).unwrap();
        assert!(native_monitor_command(&wt,"stop").unwrap().status.success());
        let report=crate::buildcache::sweep_targets_for_round(&root,std::time::Duration::ZERO).unwrap();
        assert!(!root.join(&site.target).exists(),"quiet target not swept: {report:#?}");
    }
    #[test]
    fn monitor_unknown_native_evidence_never_stops() {
        let (root,site,mut events)=fixture("b345-native-unknown");let wt=root.join(&site.worktree);let _m=Owned::start(wt.clone());
        events.retain(|e|e.kind!="ManagedWakeTerminated");write_maintenance_events(&root,"rMaint",&events);
        let result=maintain_one_site(&root,"rMaint",&events,&site,false);assert!(!result.eligible);assert!(wt.exists());monitor_status(&wt,true).unwrap();
    }
    #[test]
    fn monitor_status_must_confirm_native_stop_not_only_command_exit() {
        let (root,site,events)=fixture("b345-stop-status");let wt=root.join(&site.worktree);let _m=Owned::start(wt.clone());
        assert!(monitor_status(&wt,false).is_err());
        let result=maintain_one_site_guarded(&root,"rMaint",&events,&site,false,&||Ok(()),&|_|Ok(Command::new("/usr/bin/true").output()?));
        assert_eq!(result.disposition,"failed");assert!(wt.exists());assert_eq!(result.removed_logical_bytes,0);monitor_status(&wt,true).unwrap();
    }
    #[test]
    fn monitor_direct_ipc_owner_conflict_is_refused_before_relative_fallback() {
        use std::os::unix::process::ExitStatusExt;
        let output=|rc,out:&str|std::process::Output {status:std::process::ExitStatus::from_raw(rc<<8),stdout:out.as_bytes().to_vec(),stderr:vec![]};
        let boundary=BTreeSet::from([123]);
        assert!(monitor_endpoint_owner(&output(0,"123\n"),&boundary).unwrap());
        assert!(!monitor_endpoint_owner(&output(1,""),&boundary).unwrap());
        for observation in [output(0,"456\n"),output(1,"456\n"),output(0,"123\n456\n")] {
            let error=monitor_endpoint_owner(&observation,&boundary).unwrap_err();
            assert!(error.to_string().contains("conflicting native IPC owner"));
        }
        assert!(monitor_endpoint_owner(&output(0,""),&boundary).is_err());
    }

    #[test]
    fn monitor_pid_parser_rejects_garbage_and_ambiguous_empty() {
        use std::os::unix::process::ExitStatusExt;
        let output=|rc,out:&str,err:&str|std::process::Output{status:std::process::ExitStatus::from_raw(rc<<8),stdout:out.as_bytes().to_vec(),stderr:err.as_bytes().to_vec()};
        assert_eq!(monitor_pids(&output(1,"123\n","")).unwrap(),BTreeSet::from([123]));
        for o in [output(0,"",""),output(1,"0\n",""),output(1,"123 garbage\n",""),output(1,"123\n","warning"),output(2,"123\n","")] {assert!(monitor_pids(&o).is_err());}
        assert!(monitor_fields(b"p1\0p2\0u3\0").is_ok()); // Caller verifies exactly one process header and only descriptor records thereafter.
    }
}

// Preserve all per-owner receipts and physical identities across the all-member preflight.
// This is an observation, never a new lifecycle fact or authority to replay complete journals.
type GroupSnapshot = Vec<(PathBuf, Option<(u64, u64, bool, Vec<u8>)>)>;

fn equivalent_group_snapshot(root: &Path, round: &str, members: &[Site]) -> Result<GroupSnapshot> {
    use std::os::unix::fs::MetadataExt;
    let first = members.first().context("empty implementation group")?;
    let mut paths = vec![(no_symlink_ancestors(root, &first.worktree)?, false),
        (no_symlink_ancestors(root, &first.target)?, false)];
    for member in members {
        if member.role != SiteRole::Implement || member.identity() != first.identity()
            || member.worktree != first.worktree || member.target != first.target
            || !member.has_production_path_contract() {
            bail!("implementation group is not exact-equivalent");
        }
        let path = no_symlink_ancestors(root, &format!("coordination/runtime/site-cleanup/{round}/{}.json", member.site_id))?;
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 16 * 1024 * 1024 {
                bail!("unsafe equivalent-group journal");
            }
        }
        if let Some(journal) = read_journal(root, round, member)? {
            if journal.phase == "complete" && (root.join(&member.worktree).exists() || root.join(&member.target).exists()) {
                bail!("complete journal with reappeared equivalent group remains held");
            }
            if Some(member.generation) != members.iter().map(|s| s.generation).max() {
                bail!("nonrepresentative journal remains an independent incomplete cleanup");
            }
        }
        paths.push((path, true));
    }
    let mut snapshot = Vec::new();
    for (path, file) in paths {
        let value = match fs::symlink_metadata(&path) {
            Ok(m) => {
                if m.file_type().is_symlink() || (file && (!m.is_file() || m.len() > 16 * 1024 * 1024))
                    || (!file && !m.is_dir()) {
                    bail!("unsafe equivalent-group observation: {}", path.display());
                }
                Some((m.dev(), m.ino(), file, if file { fs::read(&path)? } else { Vec::new() }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        snapshot.push((path, value));
    }
    Ok(snapshot)
}

// All members use the same physical paths but retain independent lease/evidence/journal checks.
// The callback re-reads the unfiltered cross-round claim domain while the caller holds its lock.
pub(crate) fn maintain_equivalent_implementations(
    root: &Path, round: &str, events: &[EventRecord], members: &[Site], dry_run: bool,
    inventory_unchanged: &dyn Fn() -> bool,
) -> Vec<crate::reclaim::MaintenanceItem> {
    let representative = members.iter().max_by_key(|s| s.generation).expect("nonempty group");
    let held = |reason: String| members.iter().map(|s| crate::reclaim::MaintenanceItem::held(
        format!("{round}/{}", s.site_id), "site", vec![root.join(&s.worktree), root.join(&s.target)],
        reason.clone())).collect::<Vec<_>>();
    let before = match equivalent_group_snapshot(root, round, members) {
        Ok(snapshot) => snapshot,
        Err(e) => return held(format!("{e:#}")),
    };
    let group_guard = || -> Result<()> {
        if !inventory_unchanged() { bail!("original all-round ownership changed before group effect"); }
        for member in members {
            let preview = maintain_one_site(root, round, events, member, true);
            if !preview.eligible { bail!("group owner {}: {}", member.site_id, preview.reason); }
        }
        if equivalent_group_snapshot(root, round, members)? != before || !inventory_unchanged() {
            bail!("equivalent-group identity, receipt or ownership changed during preflight");
        }
        Ok(())
    };
    if let Err(e) = group_guard() { return held(format!("{e:#}")); }
    let mut result = maintain_one_site_guarded(root, round, events, representative, dry_run, &group_guard, &|wt| native_monitor_command(wt, "stop"));
    result.criterion("all-round-ownership", Some(true), "unfiltered all-round claims joined to every exact-equivalent owner".into());
    result.criterion("fresh-ledger-critical-section", if dry_run { None } else { Some(true) },
        "all-owner preflight and effect-boundary identity/receipt/claim recheck; no historical writes".into());
    let reason = format!("zero-deletion alias of {round}/{}; representative outcome={}: {}",
        representative.site_id, result.disposition, result.reason);
    let mut items = vec![result];
    for member in members.iter().filter(|s| s.site_id != representative.site_id) {
        let mut alias = crate::reclaim::MaintenanceItem::held(format!("{round}/{}", member.site_id),
            "site", vec![root.join(&member.worktree), root.join(&member.target)], reason.clone());
        alias.removed_logical_bytes = 0;
        items.push(alias);
    }
    items
}

/// Use the existing physical reaper only after the same read-only preview prerequisites pass.
/// Caller holds the global ledger lock for apply; this function never appends lifecycle events.
pub(crate) fn maintain_one_site(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    site: &Site,
    dry_run: bool,
) -> crate::reclaim::MaintenanceItem {
    maintain_one_site_guarded(root, round, events, site, dry_run, &|| Ok(()), &|wt| native_monitor_command(wt, "stop"))
}

fn maintain_one_site_guarded(
    root: &Path, round: &str, events: &[EventRecord], site: &Site, dry_run: bool,
    before_effect: &dyn Fn() -> Result<()>,
    stop: &dyn Fn(&Path) -> Result<std::process::Output>,
) -> crate::reclaim::MaintenanceItem {
    use std::os::unix::fs::MetadataExt;
    let mut item = crate::reclaim::MaintenanceItem::held(
        format!("{round}/{}", site.site_id),
        "site",
        vec![root.join(&site.worktree), root.join(&site.target)],
        "not eligible".into(),
    );
    let original_inventory = crate::reclaim::storage_inventory(root);
    let mut pending_monitor = None;
    let observation = (|| -> Result<(PathBuf, PathBuf, PathBuf, Vec<(PathBuf, u64, u64)>)> {
        if !site.has_production_path_contract() {
            bail!("site paths are not canonical; no adoption");
        }
        let worktree = no_symlink_ancestors(root, &site.worktree)?;
        let target = no_symlink_ancestors(root, &site.target)?;
        item.criterion(
            "canonical-no-symlink-paths",
            Some(true),
            "exact generation paths, including missing ancestors".into(),
        );
        if !matches!(
            LeaseState::of(events, &site.identity(), site.generation),
            LeaseState::Released { .. }
        ) {
            bail!("site is active, unknown, or lacks a trusted last-user release");
        }
        if orch_core::fold(events)
            .tasks
            .get(&site.task_id)
            .and_then(|t| t.state)
            == Some(orch_core::TaskState::Blocked)
        {
            bail!("BLOCKED task scene is explicitly retained");
        }
        item.criterion(
            "trusted-release-and-not-blocked",
            Some(true),
            "fresh exact lease generation and task state".into(),
        );
        let expected_head = maintenance_evidence(root, round, events, site)?;
        item.criterion(
            "native-end-and-preserved-artifact",
            Some(true),
            "fixed request/native terminal and retained bytes, or proven pre-spawn rejection"
                .into(),
        );
        if worktree.exists() {
            let marker = fs::symlink_metadata(worktree.join(".git"))
                .context("worktree lacks its own Git identity")?;
            if marker.file_type().is_symlink() || !(marker.is_file() || marker.is_dir()) {
                bail!("unsafe worktree Git identity");
            }
            if !gitx::worktree_registry(root)?
                .iter()
                .any(|e| e.path == worktree && !e.prunable)
            {
                bail!("worktree registration differs");
            }
            if let Some(head) = &expected_head {
                if gitx::rev_parse(&worktree, "HEAD")? != *head {
                    bail!("worktree fixed HEAD changed");
                }
            }
            let nested = crate::buildcache::diagnostic_cache_records(&worktree)?;
            if nested.iter().any(|v| v.disposition != "removed") {
                bail!("nested diagnostic ownership remains unresolved; maintain that root independently");
            }
            if site.role == SiteRole::Implement {
                let recorded = crate::reclaim::recorded_tasks_from_events(events);
                let criteria = crate::reclaim::observe_task_site_criteria(
                    root,
                    &worktree,
                    &site.task_id,
                    &recorded,
                );
                if let crate::reclaim::TaskSiteDisposition::Keep { reason } =
                    crate::reclaim::decide_task_site_reclaim(&criteria)
                {
                    bail!("{reason}");
                }
            }
            let (untracked, bytes) =
                physical_site_observation(root, site, &worktree)?.map_err(anyhow::Error::msg)?;
            let names = untracked.iter().map(String::as_str).collect::<Vec<_>>();
            if let ResidueDisposition::RefuseAndEscalate { bytes, limit } =
                ResidueDisposition::for_untracked(&names, bytes)
            {
                bail!("untracked residue {bytes} exceeds unchanged quarantine limit {limit}");
            }
            for name in untracked {
                let destination = format!(
                    "coordination/runtime/site-quarantine/{round}/{}/{name}",
                    site.site_id
                );
                let destination = no_symlink_ancestors(root, &destination)?;
                if fs::symlink_metadata(&destination).is_ok() {
                    bail!("quarantine destination exists; no overwrite");
                }
            }
        }
        item.criterion(
            "fixed-clean-worktree-and-quarantine-bound",
            Some(true),
            "shared physical preflight; no skipped cleanliness or enlarged residue limit".into(),
        );
        let registry = gitx::worktree_registry(root)?;
        if !worktree.exists()
            && registry.iter().any(|e| e.path == worktree)
            && registry
                .iter()
                .filter(|e| e.prunable)
                .map(|e| &e.path)
                .collect::<Vec<_>>()
                != vec![&worktree]
        {
            bail!("exact registry prune cannot be separated from other prunable owners");
        }
        let journal = read_journal(root, round, site)?;
        if journal.as_ref().is_some_and(|j| j.phase == "complete")
            && (worktree.exists() || target.exists())
            && (!complete_journal_release_is_explicit(events, round, site)
                || !complete_journal_paths_are_exclusive(events, site))
        {
            bail!("complete-journal replay lacks explicit exclusive release");
        }
        item.criterion(
            "journal-and-exact-registry",
            Some(true),
            "existing complete-journal ABA and exact prune guards retained".into(),
        );
        match maintenance_quiet(&[worktree.clone(), target.clone()]) {
            Ok(()) => item.criterion("last-user-absent", Some(true), "released producer and strict quiet".into()),
            Err(quiet) => {
                let monitor = prove_native_monitor(&worktree, &target)
                    .with_context(|| format!("{quiet:#}; dedicated monitor proof refused"))?;
                item.criterion("last-user-absent", None, "pending native monitor stop; dry-run has no effects".into());
                item.criterion("native-fsmonitor-release", None, format!("proved PID {} via {}; stop deferred to apply", monitor.pid, monitor.proof_mode));
                pending_monitor = Some(monitor);
            }
        }
        let quarantine = no_symlink_ancestors(
            root,
            &format!(
                "coordination/runtime/site-quarantine/{round}/{}",
                site.site_id
            ),
        )?;
        crate::reclaim::measure_boundaries(&[
            worktree.clone(),
            target.clone(),
            quarantine.clone(),
        ])?;
        item.criterion("logical-size", Some(true), "original boundaries and retained quarantine measured separately from filesystem free space".into());
        let mut identities = Vec::new();
        let mut identity_paths = vec![worktree.clone(), target.clone()];
        identity_paths.sort();
        identity_paths.dedup();
        for path in identity_paths {
            match fs::symlink_metadata(&path) {
                Ok(m) => identities.push((path, m.dev(), m.ino())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok((worktree, target, quarantine, identities))
    })();
    let (worktree, target, quarantine, identities) = match observation {
        Ok(value) => value,
        Err(error) => {
            item.reason = format!("{error:#}");
            item.criterion("refusal", Some(false), item.reason.clone());
            return item;
        }
    };
    item.eligible = true;
    item.reason = if pending_monitor.is_some() { "eligible pending native monitor stop; no deletion promised" } else { "eligible snapshot; apply rechecks under the ledger lock" }.into();
    item.criterion(
        "apply-lock-and-directory-recheck",
        if dry_run { None } else { Some(true) },
        "dry-run acquires no lock; apply checks fresh ledger and directory identity".into(),
    );
    if dry_run {
        return item;
    }
    let before =
        crate::reclaim::measure_boundaries(&[worktree.clone(), target.clone(), quarantine.clone()]);
    let mut entered_reaper = false;
    let mut stop_effect: Option<String> = None;
    let result = (|| -> Result<ReapOne> {
        let _ = before.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
        for (path, device, inode) in &identities {
            let m = fs::symlink_metadata(path)?;
            if m.file_type().is_symlink() || m.dev() != *device || m.ino() != *inode {
                bail!("directory replaced since maintenance observation");
            }
        }
        if maintenance_quiet(&[worktree.clone(), target.clone()]).is_err() {
            // All raw owners and the complete old static preview precede ANY stop.
            if !crate::reclaim::maintenance_inventory_unchanged(root, &original_inventory) {
                bail!("ownership changed before native stop");
            }
            before_effect()?;
            let fresh = maintain_one_site(root, round, events, site, true);
            if !fresh.eligible { bail!("site no longer eligible before native stop: {}", fresh.reason); }
            let monitor = prove_native_monitor(&worktree, &target)?;
            if pending_monitor.as_ref() != Some(&monitor) { bail!("native monitor identity changed before stop"); }
            if !crate::reclaim::maintenance_inventory_unchanged(root, &original_inventory) { bail!("ownership changed during native proof"); }
            for (path, device, inode) in &identities {
                if monitor_identity(path)? != (*device, *inode) { bail!("directory changed during native proof"); }
            }
            stop_effect = Some(format!("native monitor PID {} stop attempted via {}", monitor.pid, monitor.proof_mode));
            let stopped = stop(&worktree)?;
            if !stopped.status.success() || !stopped.stderr.is_empty() { bail!("native stop failed; outcome must be re-observed"); }
            stop_effect = Some(format!("native monitor PID {} stop command succeeded", monitor.pid));
            monitor_status(&worktree, false)?;
            match fs::symlink_metadata(&monitor.ipc) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
                _ => bail!("native stop left IPC endpoint or uncertain absence"),
            }
            maintenance_quiet(&[worktree.clone(), target.clone()])?;
            stop_effect = Some(format!("native monitor PID {} stopped; IPC absent and strict quiet confirmed", monitor.pid));
            item.criterion("native-fsmonitor-release", Some(true), stop_effect.clone().unwrap());
            item.criterion("last-user-absent", Some(true), "strict quiet after native stop".into());
            if !crate::reclaim::maintenance_inventory_unchanged(root, &original_inventory) { bail!("ownership changed after native stop"); }
            let fresh = maintain_one_site(root, round, events, site, true);
            if !fresh.eligible { bail!("post-stop evidence refused: {}", fresh.reason); }
            for (path, device, inode) in &identities {
                if monitor_identity(path)? != (*device, *inode) { bail!("post-stop directory changed"); }
            }
        }
        maintenance_quiet(&[worktree.clone(), target.clone()])?;
        before_effect()?;
        let mut outcome = ReapOutcome::default();
        let mut ignored_legacy_delta = 0;
        entered_reaper = true;
        reap_one(
            root,
            round,
            events,
            site,
            false,
            &mut outcome,
            &mut ignored_legacy_delta,
        )
    })();
    let after = crate::reclaim::measure_boundaries(&[worktree, target, quarantine]);
    if entered_reaper {
        if let (Ok(before), Ok(after)) = (before, after) {
            item.removed_logical_bytes = before.saturating_sub(after);
        }
    } // A refused preflight did not delete external/concurrently replaced bytes.
    item.logical_bytes = crate::reclaim::measure_boundaries(&item.paths).ok();
    match result {
        Ok(ReapOne::Removed | ReapOne::AlreadyComplete) => {
            item.disposition = "removed".into();
            item.reason =
                "released site reclaimed or already absent; preserved quarantine is not deletion"
                    .into();
        }
        Ok(ReapOne::Refused { reason, .. }) => {
            item.reason = reason;
            item.disposition = "held".into();
        }
        Ok(ReapOne::TargetFailed { reason, .. }) => {
            item.reason = reason;
            item.disposition = "failed".into();
        }
        Err(error) => {
            item.reason = format!("{error:#}");
            item.disposition = "failed".into();
        }
    }
    if let Some(effect) = stop_effect {
        if !item.criteria.iter().any(|c| c.name == "native-fsmonitor-release" && c.passed == Some(true)) {
            item.criterion("native-fsmonitor-release", Some(false), effect.clone());
        }
        item.reason = format!("{effect}; {}", item.reason);
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    static TEST_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn event(kind: &str, task: &str, payload: serde_json::Value) -> EventRecord {
        ledger::event(kind, "runtime:orch", Some(task), Some("rT"), payload)
    }

    fn lease(generation: u32) -> EventRecord {
        event(
            "WorkspaceLeased",
            "BT",
            serde_json::json!({
                "siteId": format!("BT-primary-executor-oc-g{generation:02}"),
                "generation": generation,
                "attemptId": "BT-A0001",
                "role": "primary",
                "agent": "executor-oc",
                "reviewedHead": "a".repeat(40),
                "wakeId": "wake-1",
                "paths": {
                    "worktree": format!(".worktrees/review-BT-A0001-primary-executor-oc-g{generation:02}"),
                    "target": format!("orch/target/review-BT-A0001-primary-executor-oc-g{generation:02}"),
                }
            }),
        )
    }

    fn release(generation: u32) -> EventRecord {
        event(
            "WorkspaceReleased",
            "BT",
            serde_json::json!({
                "siteId": format!("BT-primary-executor-oc-g{generation:02}"),
                "generation": generation,
                "attemptId": "BT-A0001",
                "role": "primary",
                "agent": "executor-oc",
                "wakeId": "wake-1",
                "completionReceipt": MANAGED_COMPLETION_RECEIPT,
            }),
        )
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_root(tag: &str) -> PathBuf {
        let sequence = TEST_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "orch-b207-sites-{tag}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("tracked.txt"), "baseline\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=orch-test",
                "-c",
                "user.email=orch-test@example.invalid",
                "commit",
                "-m",
                "baseline",
            ],
        );
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).unwrap();
        fs::write(root.join("coordination/rounds/rT/events.jsonl"), "").unwrap();
        fs::write(root.join("coordination/runtime/ledger-wal/rT.jsonl"), "").unwrap();
        root
    }

    fn activate_schema3_review_round(root: &Path) {
        if crate::round::contract_schema_at_root(root, "rT").unwrap()
            == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
        {
            return;
        }
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
        ledger::append(
            root,
            "rT",
            &[ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({
                    "contractSchemaVersion": crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION,
                    "purpose": "review-site fixture",
                }),
            )],
        )
        .unwrap();
    }

    fn provision(root: &Path, wake_id: &str) -> Site {
        activate_schema3_review_round(root);
        let head = gitx::rev_parse(root, "HEAD").unwrap();
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        fs::create_dir_all(root.join("orch/target")).unwrap();
        lease_review_site_with(
            root,
            "rT",
            "BT",
            "BT-A0001",
            SiteRole::Review,
            "executor-oc",
            &head,
            wake_id,
            |site| {
                gitx::worktree_add_detached(root, &root.join(&site.worktree), &head)?;
                fs::create_dir_all(root.join(&site.target))?;
                Ok(())
            },
        )
        .unwrap()
    }

    #[test]
    fn legacy_and_formal_review_site_writers_reject_before_provision_or_append() {
        let root = test_root("generation-reject");
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rT\n").unwrap();
        let head = gitx::rev_parse(&root, "HEAD").unwrap();
        let ledger_path = root.join("coordination/rounds/rT/events.jsonl");
        let wal_path = root.join("coordination/runtime/ledger-wal/rT.jsonl");
        let ledger_before = fs::read(&ledger_path).unwrap();
        let wal_before = fs::read(&wal_path).unwrap();
        let mut provisioned = false;
        let error = lease_review_site_with(
            &root,
            "rT",
            "BT",
            "BT-A0001",
            SiteRole::Review,
            "executor-oc",
            &head,
            "wake-legacy",
            |_| {
                provisioned = true;
                Ok(())
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("schema 3"), "{error}");
        assert!(!provisioned);
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        assert_eq!(fs::read(&wal_path).unwrap(), wal_before);

        activate_schema3_review_round(&root);
        let ledger_before = fs::read(&ledger_path).unwrap();
        let wal_before = fs::read(&wal_path).unwrap();
        let mut provisioned = false;
        let error = lease_review_site_with(
            &root,
            "rT",
            "BT",
            "BT-A0001",
            SiteRole::Primary,
            "executor-oc",
            &head,
            "wake-formal",
            |_| {
                provisioned = true;
                Ok(())
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("only accepts the schema 3 review role"),
            "{error}"
        );
        assert!(!provisioned);
        assert_eq!(fs::read(&ledger_path).unwrap(), ledger_before);
        assert_eq!(fs::read(&wal_path).unwrap(), wal_before);
        fs::remove_dir_all(root).unwrap();
    }

    fn release_site(root: &Path, site: &Site) {
        let event = ledger::event(
            "WorkspaceReleased",
            "runtime:orch",
            Some(&site.task_id),
            Some("rT"),
            serde_json::json!({
                "siteId": site.site_id,
                "generation": site.generation,
                "attemptId": site.attempt_id,
                "role": site.role.as_str(),
                "agent": site.agent,
                "wakeId": site.wake_id,
                "completionReceipt": MANAGED_COMPLETION_RECEIPT,
            }),
        );
        ledger::append(root, "rT", &[event]).unwrap();
    }

    #[test]
    fn delivery_and_recording_are_not_release_receipts() {
        let events = vec![
            lease(1),
            event("ReviewDelivered", "BT", serde_json::json!({"bodyLen": 1})),
            event("TaskRecorded", "BT", serde_json::json!({})),
        ];
        assert!(reclaimable_sites(&events).is_empty());
    }

    #[test]
    fn duplicate_or_identity_drifted_release_is_active() {
        let identity = site_identity("BT", SiteRole::Primary, "executor-oc");
        let duplicated = vec![lease(1), release(1), release(1)];
        assert!(matches!(
            LeaseState::of(&duplicated, &identity, 1),
            LeaseState::Active { .. }
        ));
        let mut drifted = release(1);
        drifted.payload.as_mut().unwrap()["attemptId"] = serde_json::json!("BT-A0002");
        assert!(matches!(
            LeaseState::of(&[lease(1), drifted], &identity, 1),
            LeaseState::Active { .. }
        ));

        for missing in ["agent", "wakeId"] {
            let mut incomplete = release(1);
            incomplete
                .payload
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(missing);
            assert!(matches!(
                LeaseState::of(&[lease(1), incomplete], &identity, 1),
                LeaseState::Active { .. }
            ));
        }
    }

    #[test]
    fn managed_termination_mints_one_exact_release_and_ambiguity_mints_none() {
        let leased = lease(1);
        let termination = event(
            "ManagedWakeTerminated",
            "BT",
            serde_json::json!({
                "wakeId": "wake-1",
                "agent": "executor-oc",
                "managedScopeTerminated": true,
            }),
        );
        let released =
            workspace_release_for_termination(std::slice::from_ref(&leased), "rT", &termination)
                .unwrap()
                .expect("exact managed termination must mint release");
        assert_eq!(payload_string(&released, "wakeId"), Some("wake-1"));
        assert_eq!(
            payload_string(&released, "terminationEventId"),
            Some(termination.event_id.as_str())
        );
        let identity = site_identity("BT", SiteRole::Primary, "executor-oc");
        assert!(matches!(
            LeaseState::of(
                &[leased.clone(), termination.clone(), released],
                &identity,
                1
            ),
            LeaseState::Released { .. }
        ));

        assert!(
            workspace_release_for_termination(&[leased.clone(), leased], "rT", &termination,)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn declared_preserve_is_skipped_by_task_record_retirement() {
        let mut preserved = lease(1);
        preserved.payload.as_mut().unwrap()["declaredPreserve"] = serde_json::json!(true);
        assert!(retire_task_sites(&[preserved], "BT", "EV-TASK-RECORDED").is_empty());
    }

    #[test]
    fn registry_identity_is_set_equality_not_only_a_count() {
        let active = parse_lease(&lease(1)).unwrap();
        let correct = worktree_registry_invariant(
            std::slice::from_ref(&active.worktree),
            std::slice::from_ref(&active),
            &[],
        );
        assert!(correct.holds);
        let wrong =
            worktree_registry_invariant(&[".worktrees/review-other".to_string()], &[active], &[]);
        assert!(!wrong.holds);
        assert_eq!(wrong.missing.len(), 1);
        assert_eq!(wrong.unexpected.len(), 1);

        let prunable = worktree_registry_invariant_with_prunable(
            &[".worktrees/review-other".to_string()],
            &[],
            &[".worktrees/review-other".to_string()],
            &[".worktrees/review-other".to_string()],
        );
        assert!(!prunable.holds);
        assert_eq!(prunable.prunable, 1);
    }

    #[test]
    fn live_fire_active_site_touches_nothing() {
        let root = test_root("active");
        let site = provision(&root, "wake-active");
        let worktree = root.join(&site.worktree);
        let target = root.join(&site.target);
        let sentinel = worktree.join("active-sentinel.txt");
        let target_sentinel = target.join("target-sentinel.txt");
        fs::write(&sentinel, "active\n").unwrap();
        fs::write(&target_sentinel, "target\n").unwrap();
        let head = gitx::rev_parse(&worktree, "HEAD").unwrap();
        let registry = gitx::worktree_registry(&root).unwrap();

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(gitx::rev_parse(&worktree, "HEAD").unwrap(), head);
        assert_eq!(fs::read(&sentinel).unwrap(), b"active\n");
        assert_eq!(fs::read(&target_sentinel).unwrap(), b"target\n");
        assert_eq!(gitx::worktree_registry(&root).unwrap(), registry);

        gitx::worktree_remove(&root, &worktree).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn released_site_quarantines_untracked_then_removes_exact_generation() {
        let root = test_root("released-quarantine");
        let site = provision(&root, "wake-released");
        let worktree = root.join(&site.worktree);
        let residue = worktree.join(".orch-review-parity/parity.sh");
        fs::create_dir_all(residue.parent().unwrap()).unwrap();
        fs::write(&residue, "#!/bin/sh\nexit 0\n").unwrap();
        release_site(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(outcome.removed, vec![site.site_id.clone()]);
        assert_eq!(outcome.quarantined, vec![site.site_id.clone()]);
        assert!(!root.join(&site.worktree).exists());
        assert!(!root.join(&site.target).exists());
        assert!(root
            .join("coordination/runtime/site-quarantine/rT")
            .join(&site.site_id)
            .join(".orch-review-parity/parity.sh")
            .exists());
        assert!(outcome.registry_invariant.unwrap().holds);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dirty_refusal_does_not_abort_a_later_released_generation() {
        let root = test_root("partial-progress");
        let dirty = provision(&root, "wake-dirty");
        release_site(&root, &dirty);
        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "dirty\n").unwrap();

        let clean = provision(&root, "wake-clean");
        release_site(&root, &clean);
        fs::write(root.join(&clean.target).join("artifact"), b"reclaim-me").unwrap();

        let report = reap_released_sites_reported(&root, "rT").unwrap();
        assert_eq!(report.reaped, vec![clean.site_id.clone()]);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.refused[0].0, dirty.site_id);
        assert!(report.refused[0].1.contains("tracked/staged"));
        assert!(report.freed_bytes > 0);
        assert!(root.join(&dirty.worktree).exists());
        assert!(root.join(&dirty.target).exists());
        assert!(!root.join(&clean.worktree).exists());
        assert!(!root.join(&clean.target).exists());

        fs::write(root.join(&dirty.worktree).join("tracked.txt"), "baseline\n").unwrap();
        gitx::worktree_remove(&root, &root.join(&dirty.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn released_site_with_busy_process_cwd_is_reaped_without_liveness_guessing() {
        let root = test_root("released-busy-cwd");
        let site = provision(&root, "wake-busy");
        let worktree = root.join(&site.worktree);
        let mut occupant = Command::new("sh")
            .args(["-c", "sleep 30"])
            .current_dir(&worktree)
            .spawn()
            .unwrap();
        release_site(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        let _ = occupant.kill();
        let _ = occupant.wait();
        assert_eq!(outcome.removed, vec![site.site_id.clone()]);
        assert!(!root.join(&site.worktree).exists());
        assert!(!root.join(&site.target).exists());
        assert!(outcome.registry_invariant.unwrap().holds);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_residue_is_escalated_and_site_is_retained() {
        let root = test_root("oversized");
        let site = provision(&root, "wake-oversized");
        let worktree = root.join(&site.worktree);
        let blob = File::create(worktree.join("blob.bin")).unwrap();
        blob.set_len(QUARANTINE_LIMIT_BYTES + 1).unwrap();
        release_site(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused.len(), 1);
        assert!(worktree.exists());
        assert!(root.join(&site.target).exists());
        let read =
            orch_core::read_ledger(&root.join("coordination/rounds/rT/events.jsonl")).unwrap();
        assert!(read.events.iter().any(|event| {
            event.kind == "EscalationRaised"
                && payload_string(event, "stage") == Some("site-cleanup-storage")
        }));

        gitx::worktree_remove(&root, &worktree).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn late_generation_waits_for_reaper_ledger_lock_and_survives() {
        let root = test_root("late-generation");
        let first = provision(&root, "wake-first");
        release_site(&root, &first);
        let head = gitx::rev_parse(&root, "HEAD").unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (provision_tx, provision_rx) = mpsc::channel();
        let handle = Arc::new(Mutex::new(None));
        let handle_from_hook = Arc::clone(&handle);
        let root_from_hook = root.clone();
        let head_from_hook = head.clone();

        let outcome = reap_released_sites_with_hook(&root, "rT", move || {
            let root = root_from_hook.clone();
            let thread = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                lease_review_site_with(
                    &root,
                    "rT",
                    "BT",
                    "BT-A0001",
                    SiteRole::Review,
                    "executor-oc",
                    &head_from_hook,
                    "wake-second",
                    |site| {
                        let _ = provision_tx.send(());
                        gitx::worktree_add_detached(
                            &root,
                            &root.join(&site.worktree),
                            &head_from_hook,
                        )?;
                        fs::create_dir_all(root.join(&site.target))?;
                        Ok(())
                    },
                )
            });
            started_rx.recv().unwrap();
            assert!(
                provision_rx
                    .recv_timeout(Duration::from_millis(100))
                    .is_err(),
                "late lease provision entered before the reaper released the ledger lock"
            );
            *handle_from_hook.lock().unwrap() = Some(thread);
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome.removed, vec![first.site_id]);
        let second = handle
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(second.generation, 2);
        assert!(root.join(&second.worktree).exists());
        let replay = reap_released_sites(&root, "rT").unwrap();
        assert!(replay.removed.iter().all(|site| site != &second.site_id));
        assert!(replay.registry_invariant.unwrap().holds);

        gitx::worktree_remove(&root, &root.join(&second.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unmapped_legacy_name_is_reported_and_never_deleted() {
        let root = test_root("legacy");
        let legacy = root.join(".worktrees/BLEG-primary-executor-old");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let head = gitx::rev_parse(&root, "HEAD").unwrap();
        gitx::worktree_add_detached(&root, &legacy, &head).unwrap();
        fs::write(legacy.join("legacy-sentinel"), "keep\n").unwrap();

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(outcome.legacy_reports.len(), 1);
        assert!(outcome.legacy_reports[0].worktree.contains("BLEG-primary"));
        assert_eq!(fs::read(legacy.join("legacy-sentinel")).unwrap(), b"keep\n");

        gitx::worktree_remove(&root, &legacy).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    // ---------------- B314 · complete-journal ABA recovery ----------------
    //
    // Non-seed companions to `tests/site_cleanup_journal_aba.rs`: the seeded
    // contract pins the public behaviour end-to-end, while the cases below pin
    // the evidence shapes the recovery must never confuse with a clean replay
    // (duplicate/drifted release, active path sharing, quarantine interruption)
    // and the two single-component recreations (registry-only, target-only).

    /// Provision, release, and tear down one site so its cleanup journal holds
    /// the original `phase == "complete"` receipt.
    fn aba_complete_once(root: &Path, wake_id: &str) -> Site {
        let site = provision(root, wake_id);
        release_site(root, &site);
        let outcome = reap_released_sites(root, "rT").unwrap();
        assert_eq!(outcome.removed, vec![site.site_id.clone()]);
        assert!(!root.join(&site.worktree).exists());
        assert!(!root.join(&site.target).exists());
        let journal = read_journal(root, "rT", &site).unwrap().unwrap();
        assert_eq!(journal.phase, "complete");
        site
    }

    /// Recreate the ABA state: the same canonical worktree checked out
    /// detached at the lease's reviewedHead, plus the exact target directory.
    fn aba_recreate_site(root: &Path, site: &Site) {
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        gitx::worktree_add_detached(root, &root.join(&site.worktree), &site.reviewed_head).unwrap();
        fs::create_dir_all(root.join(&site.target)).unwrap();
    }

    fn aba_recreate_registry_only(root: &Path, site: &Site) {
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        gitx::worktree_add_detached(root, &root.join(&site.worktree), &site.reviewed_head).unwrap();
        fs::remove_dir_all(root.join(&site.worktree)).unwrap();
    }

    fn aba_write_complete_journal(root: &Path, site: &Site) {
        write_journal(
            root,
            "rT",
            &CleanupJournal {
                version: 1,
                round: "rT".to_string(),
                site: site.clone(),
                phase: "complete".to_string(),
                quarantine: None,
                note: None,
            },
        )
        .unwrap();
    }

    fn aba_journal_bytes(root: &Path, site: &Site) -> Vec<u8> {
        fs::read(journal_path(root, "rT", site)).unwrap()
    }

    fn aba_ledger_bytes(root: &Path) -> Vec<u8> {
        fs::read(root.join("coordination/rounds/rT/events.jsonl")).unwrap()
    }

    fn aba_rewrite_ledger_and_wal(root: &Path, events: &[EventRecord]) {
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend(serde_json::to_vec(event).unwrap());
            bytes.push(b'\n');
        }
        fs::write(root.join("coordination/rounds/rT/events.jsonl"), &bytes).unwrap();
        fs::write(
            root.join("coordination/runtime/ledger-wal/rT.jsonl"),
            &bytes,
        )
        .unwrap();
    }

    fn aba_release_event(site: &Site, generation: u32) -> EventRecord {
        ledger::event(
            "WorkspaceReleased",
            "runtime:orch",
            Some(&site.task_id),
            Some("rT"),
            serde_json::json!({
                "siteId": site.site_id,
                "generation": generation,
                "attemptId": site.attempt_id,
                "role": site.role.as_str(),
                "agent": site.agent,
                "wakeId": site.wake_id,
                "completionReceipt": MANAGED_COMPLETION_RECEIPT,
            }),
        )
    }

    fn aba_later_generation_sharing_paths(site: &Site, wake_id: &str) -> Site {
        let mut later = site.clone();
        later.generation = site.generation + 1;
        later.site_id = later.identity().site_id_for(later.generation);
        later.wake_id = Some(wake_id.to_string());
        later
    }

    fn aba_managed_termination_event(site: &Site, round: &str) -> EventRecord {
        ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(&site.task_id),
            Some(round),
            serde_json::json!({
                "wakeId": site.wake_id,
                "agent": site.agent,
                "managedScopeTerminated": true,
            }),
        )
    }

    fn aba_anchored_release_event(
        site: &Site,
        round: &str,
        termination_event_id: &str,
    ) -> EventRecord {
        ledger::event(
            "WorkspaceReleased",
            "runtime:orch",
            Some(&site.task_id),
            Some(round),
            serde_json::json!({
                "siteId": site.site_id,
                "generation": site.generation,
                "attemptId": site.attempt_id,
                "role": site.role.as_str(),
                "agent": site.agent,
                "wakeId": site.wake_id,
                "completionReceipt": MANAGED_COMPLETION_RECEIPT,
                "terminationEventId": termination_event_id,
            }),
        )
    }

    #[test]
    fn complete_journal_terminal_only_does_not_authorize_aba() {
        let root = test_root("aba-terminal-only");
        let site = provision(&root, "wake-aba-terminal-only");
        aba_write_complete_journal(&root, &site);
        let termination = ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(&site.task_id),
            Some("rT"),
            serde_json::json!({
                "wakeId": site.wake_id,
                "agent": site.agent,
                "managedScopeTerminated": true,
            }),
        );
        ledger::append(&root, "rT", &[termination]).unwrap();
        assert!(matches!(
            LeaseState::of(
                &orch_core::read_ledger(&root.join("coordination/rounds/rT/events.jsonl"))
                    .unwrap()
                    .events,
                &site.identity(),
                site.generation
            ),
            LeaseState::Released { .. }
        ));
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(outcome.refused_details[0]
            .1
            .contains("WorkspaceReleased/SiteRetired"));
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_cross_round_release_does_not_authorize_aba() {
        let root = test_root("aba-cross-round-release");
        let site = provision(&root, "wake-aba-cross-round-release");
        aba_write_complete_journal(&root, &site);
        let mut release = aba_release_event(&site, site.generation);
        release.round = Some("rOther".to_string());
        ledger::append(&root, "rT", &[release]).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_cross_round_terminal_anchor_does_not_authorize_aba() {
        let root = test_root("aba-cross-round-terminal-anchor");
        let site = provision(&root, "wake-aba-cross-round-terminal-anchor");
        aba_write_complete_journal(&root, &site);
        let termination = aba_managed_termination_event(&site, "rOther");
        let release = aba_anchored_release_event(&site, "rT", &termination.event_id);
        ledger::append(&root, "rT", &[termination, release]).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_same_round_terminal_anchor_authorizes_exact_aba() {
        let root = test_root("aba-same-round-terminal-anchor");
        let site = provision(&root, "wake-aba-same-round-terminal-anchor");
        aba_write_complete_journal(&root, &site);
        let termination = aba_managed_termination_event(&site, "rT");
        let release = aba_anchored_release_event(&site, "rT", &termination.event_id);
        ledger::append(&root, "rT", &[termination, release]).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(outcome.removed, vec![site.site_id.clone()]);
        assert!(!root.join(&site.worktree).exists());
        assert!(!root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_anchored_retirement_authorizes_exact_aba() {
        let root = test_root("aba-retirement");
        let site = provision(&root, "wake-aba-retirement");
        let anchor = ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some(&site.task_id),
            Some("rT"),
            serde_json::json!({}),
        );
        let retirement = ledger::event(
            SITE_RETIRED_EVENT_KIND,
            "runtime:orch",
            Some(&site.task_id),
            Some("rT"),
            serde_json::json!({
                "siteId": site.site_id,
                "generation": site.generation,
                "taskId": site.task_id,
                "attemptId": site.attempt_id,
                "role": site.role.as_str(),
                "agent": site.agent,
                "wakeId": site.wake_id,
                "trigger": "task-recorded",
                "retireEventId": anchor.event_id,
            }),
        );
        ledger::append(&root, "rT", &[anchor, retirement]).unwrap();
        let initial = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(initial.removed, vec![site.site_id.clone()]);
        aba_recreate_site(&root, &site);
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let replay = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(replay.removed, vec![site.site_id.clone()]);
        assert!(!root.join(&site.worktree).exists());
        assert!(!root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_multi_registry_aba_reports_every_pruned_generation() {
        let root = fs::canonicalize(test_root("aba-multi-registry")).unwrap();
        let first = aba_complete_once(&root, "wake-aba-multi-1");
        let second = aba_complete_once(&root, "wake-aba-multi-2");
        aba_recreate_registry_only(&root, &first);
        aba_recreate_registry_only(&root, &second);
        // An unrelated active site must not turn the preflight into a blanket
        // "any active lease blocks every batch prune" rule.
        let unrelated = ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("BU"),
            Some("rT"),
            serde_json::json!({
                "siteId": "BU-primary-executor-oc-g01",
                "generation": 1,
                "attemptId": "BU-A0001",
                "role": "primary",
                "agent": "executor-oc",
                "reviewedHead": first.reviewed_head,
                "wakeId": "wake-aba-unrelated",
                "paths": {
                    "worktree": ".worktrees/review-BU-A0001-primary-executor-oc-g01",
                    "target": "orch/target/review-BU-A0001-primary-executor-oc-g01",
                },
            }),
        );
        ledger::append(&root, "rT", &[unrelated]).unwrap();
        assert_eq!(
            registered_worktree_snapshot(&root).unwrap().prunable.len(),
            2
        );
        let ledger_before = aba_ledger_bytes(&root);
        let first_journal = aba_journal_bytes(&root, &first);
        let second_journal = aba_journal_bytes(&root, &second);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(outcome.removed.len(), 2);
        assert_eq!(
            outcome
                .removed
                .iter()
                .filter(|site_id| *site_id == &first.site_id)
                .count(),
            1
        );
        assert_eq!(
            outcome
                .removed
                .iter()
                .filter(|site_id| *site_id == &second.site_id)
                .count(),
            1
        );
        assert_eq!(
            outcome.removed.iter().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([first.site_id.clone(), second.site_id.clone()])
        );
        let registry = registered_worktree_snapshot(&root).unwrap();
        assert!(!registry.paths.contains(&first.worktree));
        assert!(!registry.paths.contains(&second.worktree));
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &first), first_journal);
        assert_eq!(aba_journal_bytes(&root, &second), second_journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_batch_prune_waits_for_active_target_preflight() {
        let root = fs::canonicalize(test_root("aba-batch-active-target")).unwrap();
        let first = aba_complete_once(&root, "wake-aba-batch-1");
        let second = aba_complete_once(&root, "wake-aba-batch-2");
        aba_recreate_registry_only(&root, &first);
        aba_recreate_registry_only(&root, &second);
        let squatter = ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("BQ"),
            Some("rT"),
            serde_json::json!({
                "siteId": "BQ-primary-executor-oc-g01",
                "generation": 1,
                "attemptId": "BQ-A0001",
                "role": "primary",
                "agent": "executor-oc",
                "reviewedHead": first.reviewed_head,
                "wakeId": "wake-aba-batch-squatter",
                "paths": {
                    "worktree": ".worktrees/review-BQ-A0001-primary-executor-oc-g01",
                    "target": first.target,
                },
            }),
        );
        ledger::append(&root, "rT", &[squatter]).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let first_journal = aba_journal_bytes(&root, &first);
        let second_journal = aba_journal_bytes(&root, &second);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert!(outcome
            .refused_details
            .iter()
            .any(|(site_id, reason)| site_id == &first.site_id
                && reason.contains("共享重建现场路径")));
        let registry = registered_worktree_snapshot(&root).unwrap();
        assert!(registry.paths.contains(&first.worktree));
        assert!(registry.paths.contains(&second.worktree));
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &first), first_journal);
        assert_eq!(aba_journal_bytes(&root, &second), second_journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_batch_prune_waits_for_journal_identity_preflight() {
        let root = fs::canonicalize(test_root("aba-batch-journal-drift")).unwrap();
        let first = aba_complete_once(&root, "wake-aba-journal-1");
        let second = aba_complete_once(&root, "wake-aba-journal-2");
        aba_recreate_registry_only(&root, &first);
        aba_recreate_registry_only(&root, &second);
        let path = journal_path(&root, "rT", &first);
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        journal["site"]["target"] = serde_json::json!(second.target);
        fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let drifted_journal = fs::read(&path).unwrap();
        let second_journal = aba_journal_bytes(&root, &second);

        let result = reap_released_sites(&root, "rT");
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("journal identity/version drift"));
        let registry = registered_worktree_snapshot(&root).unwrap();
        assert!(registry.paths.contains(&first.worktree));
        assert!(registry.paths.contains(&second.worktree));
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(fs::read(&path).unwrap(), drifted_journal);
        assert_eq!(aba_journal_bytes(&root, &second), second_journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_registry_batch_refuses_multi_generation_path_ownership() {
        let root = fs::canonicalize(test_root("aba-registry-multi-generation")).unwrap();
        let shared = aba_complete_once(&root, "wake-aba-registry-shared");
        let other = aba_complete_once(&root, "wake-aba-registry-other");
        let mut later =
            aba_later_generation_sharing_paths(&shared, "wake-aba-registry-shared-later");
        later.generation = other.generation + 1;
        later.site_id = later.identity().site_id_for(later.generation);
        let lease = workspace_leased_event("rT", &later);
        let release = aba_release_event(&later, later.generation);
        ledger::append(&root, "rT", &[lease, release]).unwrap();
        aba_write_complete_journal(&root, &later);
        aba_recreate_registry_only(&root, &shared);
        aba_recreate_registry_only(&root, &other);
        let ledger_before = aba_ledger_bytes(&root);
        let shared_journal = aba_journal_bytes(&root, &shared);
        let other_journal = aba_journal_bytes(&root, &other);
        let later_journal = aba_journal_bytes(&root, &later);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        let registry = registered_worktree_snapshot(&root).unwrap();
        assert!(registry.paths.contains(&shared.worktree));
        assert!(registry.paths.contains(&other.worktree));
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &shared), shared_journal);
        assert_eq!(aba_journal_bytes(&root, &other), other_journal);
        assert_eq!(aba_journal_bytes(&root, &later), later_journal);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_single_site_drift_is_a_quiet_preserving_refusal() {
        let root = test_root("aba-single-journal-drift");
        let site = aba_complete_once(&root, "wake-aba-single-journal-drift");
        aba_recreate_site(&root, &site);
        let path = journal_path(&root, "rT", &site);
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        journal["site"]["target"] = serde_json::json!(format!("{}-drift", site.target));
        fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = fs::read(&path).unwrap();

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(outcome.refused_details[0]
            .1
            .contains("journal identity/version drift"));
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(fs::read(&path).unwrap(), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_lease_path_drift_is_a_quiet_preserving_refusal() {
        let root = test_root("aba-lease-path-drift");
        let site = aba_complete_once(&root, "wake-aba-lease-path-drift");
        aba_recreate_site(&root, &site);
        let ledger_path = root.join("coordination/rounds/rT/events.jsonl");
        let mut events = orch_core::read_ledger(&ledger_path).unwrap().events;
        let lease = events
            .iter_mut()
            .find(|event| event.kind == "WorkspaceLeased")
            .unwrap();
        lease.payload.as_mut().unwrap()["paths"]["target"] =
            serde_json::json!("orch/target/noncanonical-aba-drift");
        aba_rewrite_ledger_and_wal(&root, &events);
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(outcome.refused_details[0]
            .1
            .contains("journal identity/version drift"));
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_aba_with_duplicate_release_preserves_site_journal_and_ledger() {
        let root = test_root("aba-duplicate-release");
        let site = aba_complete_once(&root, "wake-aba-dup");
        aba_recreate_site(&root, &site);
        // A second release for the exact same generation makes the evidence
        // ambiguous; the fold must fail closed before any physical read, so
        // the replay never starts.
        ledger::append(&root, "rT", &[aba_release_event(&site, site.generation)]).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_aba_with_wrong_generation_release_preserves_everything() {
        let root = test_root("aba-wrong-generation");
        let site = aba_complete_once(&root, "wake-aba-gen");
        aba_recreate_site(&root, &site);
        // A release that names the site but a different generation never
        // recovers: no lease owns that key, so the evidence fails closed.
        ledger::append(
            &root,
            "rT",
            &[aba_release_event(&site, site.generation + 1)],
        )
        .unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_aba_is_refused_while_another_generation_shares_the_paths() {
        let root = test_root("aba-active-share");
        let site = aba_complete_once(&root, "wake-aba-share");
        aba_recreate_site(&root, &site);
        // A hand-built second-generation lease that squats on the exact g01
        // paths is active evidence on the same canonical location; the replay
        // must refuse rather than delete out from under it.
        let squatter = ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some("BT"),
            Some("rT"),
            serde_json::json!({
                "siteId": site.identity().site_id_for(site.generation + 1),
                "generation": site.generation + 1,
                "attemptId": site.attempt_id,
                "role": site.role.as_str(),
                "agent": site.agent,
                "reviewedHead": site.reviewed_head,
                "wakeId": "wake-aba-share-2",
                "paths": {"worktree": site.worktree, "target": site.target},
            }),
        );
        ledger::append(&root, "rT", &[squatter]).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert_eq!(outcome.refused_details[0].0, site.site_id);
        assert!(outcome.refused_details[0].1.contains("共享重建现场路径"));
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_later_terminal_generation_cannot_claim_shared_paths() {
        let root = test_root("aba-terminal-generation-share");
        let site = aba_complete_once(&root, "wake-aba-terminal-generation-share");
        let later =
            aba_later_generation_sharing_paths(&site, "wake-aba-terminal-generation-share-later");
        let lease = workspace_leased_event("rT", &later);
        let termination = aba_managed_termination_event(&later, "rT");
        ledger::append(&root, "rT", &[lease, termination]).unwrap();
        aba_write_complete_journal(&root, &later);
        aba_recreate_site(&root, &site);
        let ledger_before = aba_ledger_bytes(&root);
        let site_journal = aba_journal_bytes(&root, &site);
        let later_journal = aba_journal_bytes(&root, &later);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), site_journal);
        assert_eq!(aba_journal_bytes(&root, &later), later_journal);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_later_explicit_generation_never_claims_shared_paths() {
        let root = test_root("aba-explicit-generation-share");
        let site = aba_complete_once(&root, "wake-aba-explicit-generation-share");
        let later =
            aba_later_generation_sharing_paths(&site, "wake-aba-explicit-generation-share-later");
        let lease = workspace_leased_event("rT", &later);
        let release = aba_release_event(&later, later.generation);
        ledger::append(&root, "rT", &[lease, release]).unwrap();
        aba_write_complete_journal(&root, &later);
        aba_recreate_site(&root, &site);
        let ledger_before = aba_ledger_bytes(&root);
        let site_journal = aba_journal_bytes(&root, &site);
        let later_journal = aba_journal_bytes(&root, &later);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), site_journal);
        assert_eq!(aba_journal_bytes(&root, &later), later_journal);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_aba_quarantine_interruption_preserves_site_and_journal() {
        let root = test_root("aba-quarantine-blocked");
        let site = aba_complete_once(&root, "wake-aba-quar");
        aba_recreate_site(&root, &site);
        fs::write(root.join(&site.worktree).join("a-first.txt"), "first\n").unwrap();
        fs::write(root.join(&site.worktree).join("z-blocked.txt"), "second\n").unwrap();
        let (inventory, _) = untracked_inventory(&root.join(&site.worktree)).unwrap();
        assert_eq!(inventory.len(), 2);
        let blocked_relative = inventory.last().unwrap();
        // A collision on the later destination must be discovered before the
        // earlier residue moves: immutable replay cannot journal partial
        // quarantine progress for crash recovery.
        let blocked = root
            .join("coordination/runtime/site-quarantine/rT")
            .join(&site.site_id)
            .join(blocked_relative);
        fs::create_dir_all(blocked.parent().unwrap()).unwrap();
        fs::write(&blocked, "already here\n").unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert_eq!(outcome.refused_details[0].0, site.site_id);
        assert!(outcome.refused_details[0]
            .1
            .contains("quarantine destination 已存在"));
        assert_eq!(
            fs::read(root.join(&site.worktree).join("a-first.txt")).unwrap(),
            b"first\n"
        );
        assert_eq!(
            fs::read(root.join(&site.worktree).join("z-blocked.txt")).unwrap(),
            b"second\n"
        );
        assert_eq!(fs::read(&blocked).unwrap(), b"already here\n");
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_dirty_refusal_is_ledger_byte_stable() {
        let root = test_root("aba-dirty-ledger");
        let site = aba_complete_once(&root, "wake-aba-dirty-ledger");
        aba_recreate_site(&root, &site);
        fs::write(root.join(&site.worktree).join("tracked.txt"), "dirty\n").unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        git(&root.join(&site.worktree), &["restore", "tracked.txt"]);
        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_foreign_common_dir_is_preserved_without_events() {
        let root = test_root("aba-foreign-common-dir");
        let site = aba_complete_once(&root, "wake-aba-foreign-common-dir");
        let foreign = root.join(&site.worktree);
        fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        git(
            &root,
            &["clone", "--no-checkout", ".", site.worktree.as_str()],
        );
        git(
            &foreign,
            &["checkout", "--detach", site.reviewed_head.as_str()],
        );
        assert!(!gitx::same_common_dir(&root, &foreign).unwrap());
        assert!(gitx::worktree_is_detached(&foreign).unwrap());
        assert_eq!(
            gitx::rev_parse(&foreign, "HEAD").unwrap(),
            site.reviewed_head
        );
        assert!(gitx::tracked_porcelain(&foreign).unwrap().is_empty());
        fs::write(foreign.join("sentinel.txt"), "foreign\n").unwrap();
        fs::create_dir_all(root.join(&site.target)).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert_eq!(
            fs::read(foreign.join("sentinel.txt")).unwrap(),
            b"foreign\n"
        );
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_attached_head_is_preserved_without_events() {
        let root = test_root("aba-attached-head");
        let site = aba_complete_once(&root, "wake-aba-attached-head");
        aba_recreate_site(&root, &site);
        git(
            &root.join(&site.worktree),
            &["switch", "-c", "aba-attached"],
        );
        assert!(gitx::same_common_dir(&root, &root.join(&site.worktree)).unwrap());
        assert!(!gitx::worktree_is_detached(&root.join(&site.worktree)).unwrap());
        assert_eq!(
            gitx::rev_parse(&root.join(&site.worktree), "HEAD").unwrap(),
            site.reviewed_head
        );
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(root.join(&site.worktree).exists());
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);

        gitx::worktree_remove(&root, &root.join(&site.worktree)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn complete_journal_worktree_symlink_preserves_link_sentinel_and_ledger() {
        use std::os::unix::fs::symlink;

        let root = test_root("aba-worktree-symlink");
        let site = aba_complete_once(&root, "wake-aba-worktree-symlink");
        let outside = root.join("outside-worktree-sentinel");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "keep\n").unwrap();
        fs::create_dir_all(root.join(&site.worktree).parent().unwrap()).unwrap();
        symlink(&outside, root.join(&site.worktree)).unwrap();
        fs::create_dir_all(root.join(&site.target)).unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert!(outcome.removed.is_empty());
        assert_eq!(outcome.refused_details.len(), 1);
        assert!(outcome.refused_details[0].1.contains("symlink component"));
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep\n");
        assert!(fs::symlink_metadata(root.join(&site.worktree))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(root.join(&site.worktree)).unwrap(), outside);
        assert!(root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_registry_only_aba_is_pruned_without_events() {
        // Canonicalize the scratch root so Git's own registry paths (always
        // canonical) compare equal to the join the reaper performs.
        let root = fs::canonicalize(test_root("aba-registry-only")).unwrap();
        let site = aba_complete_once(&root, "wake-aba-reg");
        // Recreate only the registry half of the A: a detached worktree whose
        // directory then disappears behind Git's back, leaving its entry
        // immediately prunable.
        aba_recreate_registry_only(&root, &site);
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(outcome.removed, vec![site.site_id.clone()]);
        assert!(!registered_worktree_snapshot(&root)
            .unwrap()
            .paths
            .contains(&site.worktree));
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_journal_target_only_aba_is_removed_without_events() {
        let root = test_root("aba-target-only");
        let site = aba_complete_once(&root, "wake-aba-tgt");
        // Recreate only the target half of the A.
        fs::create_dir_all(root.join(&site.target)).unwrap();
        fs::write(root.join(&site.target).join("artifact.bin"), b"reclaim me").unwrap();
        let ledger_before = aba_ledger_bytes(&root);
        let journal_before = aba_journal_bytes(&root, &site);

        let outcome = reap_released_sites(&root, "rT").unwrap();
        assert_eq!(outcome.removed, vec![site.site_id.clone()]);
        assert!(!root.join(&site.target).exists());
        assert_eq!(aba_ledger_bytes(&root), ledger_before);
        assert_eq!(aba_journal_bytes(&root, &site), journal_before);
        fs::remove_dir_all(root).unwrap();
    }
}

/// Read-only three-state site inventory. Callers reconcile late backend facts
/// before taking this snapshot; the report itself has no write path.
pub fn reclaim_report(root: &Path, events: &[EventRecord]) -> Result<Vec<ReclaimReportEntry>> {
    let mut report = Vec::new();
    let mut leased_paths = BTreeSet::new();
    for (identity, generation) in lease_identities(events) {
        match LeaseState::of(events, &identity, generation) {
            LeaseState::Released {
                site,
                completion_receipt,
            } => {
                leased_paths.insert(site.worktree.clone());
                report.push(ReclaimReportEntry {
                    state: ReclaimReportState::Reclaimable,
                    site_id: site.site_id,
                    worktree: site.worktree,
                    reason: format!("durable terminal receipt: {completion_receipt}"),
                });
            }
            LeaseState::Active { site, reason } => {
                let site_id = identity.site_id_for(generation);
                let worktree = site
                    .as_ref()
                    .map(|site| site.worktree.clone())
                    .unwrap_or_else(|| "<unknown>".to_string());
                if worktree != "<unknown>" {
                    leased_paths.insert(worktree.clone());
                }
                report.push(ReclaimReportEntry {
                    state: ReclaimReportState::Active,
                    site_id,
                    worktree,
                    reason,
                });
            }
            LeaseState::Absent => {}
        }
    }
    for worktree in registered_worktree_snapshot(root)?.paths {
        if worktree != "."
            && looks_like_legacy_review_site(&worktree)
            && !leased_paths.contains(&worktree)
        {
            report.push(ReclaimReportEntry {
                state: ReclaimReportState::Legacy,
                site_id: worktree.clone(),
                worktree,
                reason: "无可验证 WorkspaceLeased 映射；discovery-only".to_string(),
            });
        }
    }
    report.sort();
    report.dedup();
    Ok(report)
}

/// Pure ledger fold.  It intentionally performs no filesystem or process IO.
/// Keep this function at the end of the module: the frozen contract scans its
/// complete source suffix to ensure the predicate never grows an ambient
/// liveness heuristic.
pub fn reclaimable_sites(events: &[EventRecord]) -> Vec<Site> {
    lease_identities(events)
        .into_iter()
        .filter_map(
            |(identity, generation)| match LeaseState::of(events, &identity, generation) {
                LeaseState::Released { site, .. } => Some(site),
                LeaseState::Absent | LeaseState::Active { .. } => None,
            },
        )
        .collect()
}
