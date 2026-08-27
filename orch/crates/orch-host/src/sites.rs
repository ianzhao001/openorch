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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SiteRole {
    Primary,
    Secondary,
    Nongate,
    Implement,
}

impl SiteRole {
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value {
            "primary" => Ok(Self::Primary),
            "secondary" => Ok(Self::Secondary),
            "nongate" => Ok(Self::Nongate),
            "implement" => Ok(Self::Implement),
            other => Err(format!(
                "site role 只接受 primary/secondary/nongate/implement，收到 {other:?}"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Nongate => "nongate",
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
            SiteRole::Primary | SiteRole::Secondary | SiteRole::Nongate => {
                format!(".worktrees/{}", self.basename())
            }
        }
    }

    pub fn expected_target(&self) -> String {
        match self.role {
            SiteRole::Implement => format!(".worktrees/{}/orch/target", self.task_id),
            SiteRole::Primary | SiteRole::Secondary | SiteRole::Nongate => {
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
            Some("task-recorded" | "round-close" | "manual")
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

/// Allocate and durably lease one generation while holding the round ledger
/// lock.  `provision` executes inside that same lock, which serializes it with
/// teardown's fresh read and closes the late-lease deletion race.
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
    validate_site_request(task_id, attempt_id, role, agent, reviewed_head, wake_id)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取 site lease 账本失败: {}", ledger_path.display()))?;
    if !read.bad_lines.is_empty() {
        bail!("site lease 拒绝坏账本");
    }

    let identity = site_identity(task_id, role, agent);
    let mut selected = None::<Site>;
    let mut provision = Some(provision);
    ledger::append_checked(root, round, |events| {
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
    Ok(cursor)
}

fn untracked_inventory(worktree: &Path) -> Result<(Vec<String>, u64)> {
    let paths = gitx::untracked_paths(worktree)?;
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
    fs::create_dir_all(&quarantine)?;
    for relative in entries {
        let source = worktree.join(relative);
        let destination = quarantine.join(relative);
        let parent = destination
            .parent()
            .context("quarantine destination lacks parent")?;
        fs::create_dir_all(parent)?;
        if destination.exists() {
            bail!(
                "quarantine destination 已存在，拒绝覆盖: {}",
                destination.display()
            );
        }
        fs::rename(&source, &destination).with_context(|| {
            format!(
                "隔离 untracked residue 失败: {} -> {}",
                source.display(),
                destination.display()
            )
        })?;
    }
    Ok(quarantine)
}

enum ReapOne {
    Removed,
    AlreadyComplete,
    Refused(String),
    TargetFailed(String),
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
    events: &[EventRecord],
    released: &[Site],
) -> Result<()> {
    let registry = gitx::worktree_registry(root)?;
    let prunable = registry
        .iter()
        .filter(|entry| entry.prunable)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    if prunable.len() < 2 {
        return Ok(());
    }

    let active_paths = active_sites(events)
        .into_iter()
        .map(|site| site.worktree)
        .collect::<BTreeSet<_>>();
    let mut eligible = BTreeSet::new();
    for site in released {
        if !site.has_production_path_contract() || active_paths.contains(&site.worktree) {
            continue;
        }
        let worktree = no_symlink_ancestors(root, &site.worktree)?;
        let _target = no_symlink_ancestors(root, &site.target)?;
        if !worktree.exists() {
            eligible.insert(worktree);
        }
    }
    if !prunable.is_subset(&eligible) {
        // Git exposes only a global prune operation.  Leave the registry
        // untouched unless every candidate belongs to this exact released set;
        // reap_one will surface the per-site refusal and the invariant stays red.
        return Ok(());
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
    Ok(())
}

fn reap_one(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    site: &Site,
    outcome: &mut ReapOutcome,
    freed_bytes: &mut u64,
) -> Result<ReapOne> {
    if !site.has_production_path_contract() {
        return Ok(ReapOne::Refused(
            "lease paths 不符合 canonical generation 命名；只报告不删除".to_string(),
        ));
    }
    if site.role == SiteRole::Implement {
        if let Some(active) = active_sites(events)
            .into_iter()
            .find(|active| active.worktree == site.worktree)
        {
            return Ok(ReapOne::Refused(format!(
                "active lease {} shares worktree {}",
                active.site_id, site.worktree
            )));
        }
    }
    let worktree = no_symlink_ancestors(root, &site.worktree)?;
    let target = no_symlink_ancestors(root, &site.target)?;
    if site.role == SiteRole::Implement && worktree.exists() {
        let recorded = crate::reclaim::recorded_tasks_from_events(events);
        let criteria =
            crate::reclaim::observe_task_site_criteria(root, &worktree, &site.task_id, &recorded);
        if let crate::reclaim::TaskSiteDisposition::Keep { reason } =
            crate::reclaim::decide_task_site_reclaim(&criteria)
        {
            return Ok(ReapOne::Refused(format!(
                "task site criteria not met: {reason}"
            )));
        }
    }
    let mut journal = read_journal(root, round, site)?.unwrap_or(CleanupJournal {
        version: 1,
        round: round.to_string(),
        site: site.clone(),
        phase: "planned".to_string(),
        quarantine: None,
        note: None,
    });
    if journal.phase == "complete" {
        let still_registered = gitx::worktree_registry(root)?
            .iter()
            .any(|entry| entry.path == worktree);
        if worktree.exists() || target.exists() || still_registered {
            bail!(
                "complete site cleanup journal 与物理/registry 状态冲突: {}",
                site.site_id
            );
        }
        return Ok(ReapOne::AlreadyComplete);
    }
    write_journal(root, round, &journal)?;
    let bytes_before = reclaim_boundary_bytes(&worktree, &target)?;

    if worktree.exists() {
        if site.role != SiteRole::Implement {
            if !gitx::same_common_dir(root, &worktree)?
                || !gitx::worktree_is_detached(&worktree)?
                || gitx::rev_parse(&worktree, "HEAD")? != site.reviewed_head
            {
                return Ok(ReapOne::Refused(
                    "worktree common-dir/detached HEAD 与 lease 不一致".to_string(),
                ));
            }
        }
        let tracked = gitx::tracked_porcelain(&worktree)?;
        if !tracked.is_empty() {
            return Ok(ReapOne::Refused(
                "现场含 tracked/staged 修改；拒绝自动拆除".to_string(),
            ));
        }
        let (untracked, bytes) = untracked_inventory(&worktree)?;
        let untracked_refs = untracked.iter().map(String::as_str).collect::<Vec<_>>();
        match ResidueDisposition::for_untracked(&untracked_refs, bytes) {
            ResidueDisposition::Proceed => {}
            ResidueDisposition::Quarantine { .. } => {
                let quarantine = quarantine_entries(root, round, site, &worktree, &untracked)?;
                journal.phase = "quarantined".to_string();
                journal.quarantine = Some(quarantine.display().to_string());
                write_journal(root, round, &journal)?;
                outcome.quarantined.push(site.site_id.clone());
            }
            ResidueDisposition::RefuseAndEscalate { bytes, limit } => {
                return Ok(ReapOne::Refused(format!(
                    "untracked residue {bytes} bytes 超过 quarantine 上限 {limit}"
                )))
            }
        }
        if let Err(error) = gitx::worktree_remove(root, &worktree) {
            add_measured_reclaim_delta(&worktree, &target, bytes_before, freed_bytes)?;
            journal.note = Some(format!("worktree remove failed: {error:#}"));
            write_journal(root, round, &journal)?;
            return Ok(ReapOne::Refused(
                "worktree remove 失败；按契约不触碰 target".to_string(),
            ));
        }
    }
    if let Err(error) = gitx::worktree_prune_exact(root, &worktree) {
        add_measured_reclaim_delta(&worktree, &target, bytes_before, freed_bytes)?;
        return Err(error);
    }
    journal.phase = "worktree-removed".to_string();
    if let Err(error) = write_journal(root, round, &journal) {
        add_measured_reclaim_delta(&worktree, &target, bytes_before, freed_bytes)?;
        return Err(error);
    }

    let target_failure = if target.exists() {
        match crate::util::remove_dir_all_with_enotempty_retry(&target) {
            Ok(()) => None,
            Err(error) => Some(format!("target remove failed: {error}")),
        }
    } else {
        None
    };
    if let Some(note) = target_failure {
        add_measured_reclaim_delta(&worktree, &target, bytes_before, freed_bytes)?;
        journal.phase = "target-remove-failed".to_string();
        journal.note = Some(note.clone());
        write_journal(root, round, &journal)?;
        return Ok(ReapOne::TargetFailed(note));
    }
    journal.phase = "complete".to_string();
    journal.note = None;
    if let Err(error) = write_journal(root, round, &journal) {
        add_measured_reclaim_delta(&worktree, &target, bytes_before, freed_bytes)?;
        return Err(error);
    }
    add_measured_reclaim_delta(&worktree, &target, bytes_before, freed_bytes)?;
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
        prune_released_registry_batch(root, events, &released)?;
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
            let result = reap_one(root, round, events, &site, &mut outcome, &mut freed_bytes);
            outcome.freed_bytes = outcome.freed_bytes.saturating_add(freed_bytes);
            match result {
                Ok(ReapOne::Removed) => outcome.removed.push(site.site_id.clone()),
                Ok(ReapOne::AlreadyComplete) => {}
                Ok(ReapOne::Refused(reason)) => {
                    outcome.refused.push(format!("{}: {reason}", site.site_id));
                    outcome
                        .refused_details
                        .push((site.site_id.clone(), reason.clone()));
                    if !refusal_is_recorded(events, &site, &reason) {
                        escalations.push(refusal_event(round, &site, &reason));
                    }
                }
                Ok(ReapOne::TargetFailed(reason)) => {
                    outcome
                        .target_failures
                        .push(format!("{}: {reason}", site.site_id));
                    outcome
                        .refused_details
                        .push((site.site_id.clone(), reason.clone()));
                    if !refusal_is_recorded(events, &site, &reason) {
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

    fn provision(root: &Path, wake_id: &str) -> Site {
        let head = gitx::rev_parse(root, "HEAD").unwrap();
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        fs::create_dir_all(root.join("orch/target")).unwrap();
        lease_review_site_with(
            root,
            "rT",
            "BT",
            "BT-A0001",
            SiteRole::Primary,
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
                    SiteRole::Primary,
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
