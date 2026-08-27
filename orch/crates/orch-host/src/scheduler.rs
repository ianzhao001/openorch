//! Pure DAG/action scheduler seam (r43/B91).
//!
//! The planner pre-places this module so the executor can implement it without
//! touching the shared `lib.rs` hot spot.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result as AnyResult};
use fd_lock::RwLock;
use orch_core::EventRecord;

/// A task to be scheduled: identity, agent lane, quota-domain lane, and writeSet.
#[derive(Debug, Clone)]
pub struct SchedulerTask {
    pub id: String,
    pub agent: String,
    pub quota_domain: String,
    pub write_set: Vec<String>,
}

/// Snapshot of occupied capacity at the time of planning.
#[derive(Debug, Clone)]
pub struct SchedulerSnapshot {
    pub recorded: BTreeSet<String>,
    pub active_agents: HashSet<String>,
    pub active_quota_domains: HashSet<String>,
}

/// Counted capacity limits for agent and quota-domain lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityLimits {
    pub agents: BTreeMap<String, usize>,
    pub quota_domains: BTreeMap<String, usize>,
}

/// Counted scheduler snapshot. Recorded tasks are skipped; active counts consume
/// capacity before any task in the current plan is considered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacitySnapshot {
    pub recorded: BTreeSet<String>,
    pub active_agent_counts: BTreeMap<String, usize>,
    pub active_quota_counts: BTreeMap<String, usize>,
}

/// Runtime work consumes the same per-agent capacity regardless of whether it
/// is an implementation or a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoadKind {
    Implementation,
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
    pub task_id: String,
    pub kind: LoadKind,
    pub attempt_id: String,
    pub started_at: String,
    pub role: Option<String>,
}

/// One runtime admission resolved against the immutable domain projection in
/// the signed ROUND-IR. The field set is deliberately complete: this public
/// contract is frozen by B234's seeded integration test.
#[derive(Debug, Clone, Copy)]
pub struct DomainAdmission<'a> {
    pub agent: &'a str,
    pub domain: &'a str,
    pub domain_members: &'a [String],
    pub agent_capacity: usize,
    pub domain_capacity: usize,
}

/// Dependency-specific dispatch decision.  An unfinished dependency and a
/// completed dependency that is absent from the attempt baseline require
/// different operator actions, so they remain distinct typed outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyAdmission {
    Admitted,
    BlockedByDependency {
        blockers: Vec<String>,
    },
    ForwardBaselineRequired {
        dependency: String,
        merge_sha: String,
        attempt_base_sha: String,
    },
}

/// Decide whether a task's declared dependencies admit one concrete attempt
/// baseline.  Missing dependencies take precedence over stale-baseline
/// repair so callers never conflate "not finished" with "finished, but this
/// tree is old".  Both dependency order and duplicate declarations are kept.
pub fn dependency_dispatch_admission(
    depends_on: &[String],
    recorded_with_merge: &[(String, String)],
    attempt_base_sha: Option<&str>,
    base_contains: &dyn Fn(&str) -> bool,
) -> DependencyAdmission {
    let recorded = recorded_with_merge
        .iter()
        .map(|(task, _)| task.clone())
        .collect::<Vec<_>>();
    let blockers = crate::plan::task_dependency_blockers(depends_on, &recorded);
    if !blockers.is_empty() {
        return DependencyAdmission::BlockedByDependency { blockers };
    }

    let Some(attempt_base_sha) = attempt_base_sha else {
        return DependencyAdmission::Admitted;
    };
    for dependency in depends_on {
        if let Some((_, merge_sha)) = recorded_with_merge
            .iter()
            .find(|(task, _)| task == dependency)
        {
            if !base_contains(merge_sha) {
                return DependencyAdmission::ForwardBaselineRequired {
                    dependency: dependency.clone(),
                    merge_sha: merge_sha.clone(),
                    attempt_base_sha: attempt_base_sha.to_string(),
                };
            }
        }
    }

    DependencyAdmission::Admitted
}

/// Exact identity of one review slot.  Review delivery is intentionally keyed
/// by all four fields: task-only matching can release the other role's slot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeliveredReviewKey {
    pub task_id: String,
    pub attempt_id: String,
    pub role: String,
    pub agent: String,
}

/// Deterministic set of review slots that should receive a durable
/// `ReviewDelivered` fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDeliveryPlan {
    pub release: Vec<DeliveredReviewKey>,
}

/// Pure reconciliation seam used by the CLI wake path.
///
/// `requested` preserves ledger request order; therefore the result does too.
/// Filesystem observation is supplied by the caller in `present`, while
/// `already_delivered` makes repeated reconciliation idempotent.
pub fn reconcile_delivered_reviews(
    requested: &[DeliveredReviewKey],
    present: &[DeliveredReviewKey],
    already_delivered: &[DeliveredReviewKey],
) -> ReviewDeliveryPlan {
    let present = present.iter().collect::<BTreeSet<_>>();
    let already_delivered = already_delivered.iter().collect::<BTreeSet<_>>();
    ReviewDeliveryPlan {
        release: requested
            .iter()
            .filter(|key| present.contains(key) && !already_delivered.contains(key))
            .cloned()
            .collect(),
    }
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
    let mut panel_reviews =
        BTreeMap::<(String, String, String, u32), (String, LoadItem)>::new();
    let mut panel_wakes = BTreeMap::<
        String,
        ((String, String, String, u32), String),
    >::new();
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
                                && candidate.task_id.as_deref() == Some(pending.task_id.as_str())
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
                let Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatRouted(route)) = decoded
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
                let decoded = crate::ledger::decode_runtime_event_v1(event)
                    .map_err(|error| format!("ReviewSeatTerminated decode failed: {error:#}"))?;
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
                let Some((routed_key, routed_agent)) = panel_wakes.get(&terminal.wake_id) else {
                    return Err("ReviewSeatTerminated lacks routed capacity identity".to_string());
                };
                if event.actor != "runtime:orch"
                    || event.round.as_deref() != Some(round)
                    || routed_key != &key
                    || routed_agent != &terminal.agent
                {
                    return Err("ReviewSeatTerminated capacity authority is not exact".to_string());
                }
                panel_reviews.remove(&key);
            }
            "ManagedWakeTerminated" => {
                let Some(wake_id) = event_payload_string(event, "wakeId") else {
                    continue;
                };
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
                    return Err("panel ManagedWakeTerminated authority is not exact".to_string());
                }
                panel_reviews.remove(key);
            }
            _ => {}
        }
    }

    let mut load = BTreeMap::<String, Vec<LoadItem>>::new();
    for (agent, item) in implementations
        .into_values()
        .chain(reviews.into_values())
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

/// Serialize capacity-consuming runtime entry points across processes. The
/// lock covers admission through durable publication, closing the otherwise
/// unavoidable read/spawn/append race between dispatch and review wake.
pub fn with_capacity_lock<T>(root: &Path, action: impl FnOnce() -> AnyResult<T>) -> AnyResult<T> {
    let lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&lock_dir).context("创建 capacity lock 目录失败")?;
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_dir.join("capacity.lock"))
        .context("打开 capacity lock 失败")?;
    let mut lock = RwLock::new(lock_file);
    let _guard = match lock.try_write() {
        Ok(guard) => guard,
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            bail!("capacity admission lease busy：fail-fast 拒绝并发派发/审查注入")
        }
        Err(error) => return Err(error).context("获取 capacity admission lease 失败"),
    };
    action()
}

/// The action the scheduler recommends for a task.
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerAction {
    Dispatch(String),
    Wait { task: String, blockers: Vec<String> },
}

/// Plan actions for a set of tasks given current snapshot.
///
/// Rules (see task card B91):
/// 1. DAG edges only for `i < j` with writeSet/glob conflict (reuse `preset::write_sets_overlap_glob`).
/// 2. Conflicting predecessor not Recorded ⇒ Wait with `task:<ID>` blockers; Recorded tasks skipped.
/// 3. agent/quotaDomain are capacity lanes (default 1 each), not DAG dependencies.
/// 4. Within one plan, dispatched tasks immediately reserve their agent/quota lane.
/// 5. Independent tasks not blocked by slow predecessors or other lanes.
/// 6. Duplicate taskId, empty agent, empty quotaDomain ⇒ Err containing the offending taskId.
pub fn plan_actions(
    tasks: &[SchedulerTask],
    snapshot: &SchedulerSnapshot,
) -> Result<Vec<SchedulerAction>, String> {
    let mut agents = BTreeMap::new();
    let mut quota_domains = BTreeMap::new();
    for task in tasks {
        agents.entry(task.agent.clone()).or_insert(1);
        quota_domains.entry(task.quota_domain.clone()).or_insert(1);
    }
    // Preserve the legacy max=1 behavior even when the active lane is not
    // referenced by a task in this particular planning call.
    for agent in &snapshot.active_agents {
        agents.entry(agent.clone()).or_insert(1);
    }
    for quota in &snapshot.active_quota_domains {
        quota_domains.entry(quota.clone()).or_insert(1);
    }

    plan_actions_with_capacity(
        tasks,
        &CapacitySnapshot {
            recorded: snapshot.recorded.clone(),
            active_agent_counts: snapshot
                .active_agents
                .iter()
                .map(|agent| (agent.clone(), 1))
                .collect(),
            active_quota_counts: snapshot
                .active_quota_domains
                .iter()
                .map(|quota| (quota.clone(), 1))
                .collect(),
        },
        &CapacityLimits {
            agents,
            quota_domains,
        },
    )
}

/// Capacity-aware action planner.
///
/// This keeps the DAG semantics of [`plan_actions`] while replacing boolean
/// lane occupancy with explicit counts. Every Dispatch immediately reserves
/// one agent slot and one quota-domain slot for later tasks in the same plan.
pub fn plan_actions_with_capacity(
    tasks: &[SchedulerTask],
    snapshot: &CapacitySnapshot,
    limits: &CapacityLimits,
) -> Result<Vec<SchedulerAction>, String> {
    // Fail closed before returning any partial plan.
    let mut seen_ids = HashSet::new();
    for t in tasks {
        if !seen_ids.insert(&t.id) {
            return Err(format!("duplicate taskId: {}", t.id));
        }
        if t.agent.trim().is_empty() {
            return Err(format!("empty agent for taskId: {}", t.id));
        }
        if t.quota_domain.trim().is_empty() {
            return Err(format!("empty quotaDomain for taskId: {}", t.id));
        }
        match limits.agents.get(&t.agent) {
            Some(capacity) if *capacity > 0 => {}
            Some(_) => {
                return Err(format!("task {} agent {} capacity is 0", t.id, t.agent));
            }
            None => {
                return Err(format!(
                    "task {} agent {} has unknown capacity",
                    t.id, t.agent
                ));
            }
        }
        match limits.quota_domains.get(&t.quota_domain) {
            Some(capacity) if *capacity > 0 => {}
            Some(_) => {
                return Err(format!(
                    "task {} quota {} capacity is 0",
                    t.id, t.quota_domain
                ));
            }
            None => {
                return Err(format!(
                    "task {} quota {} has unknown capacity",
                    t.id, t.quota_domain
                ));
            }
        }
    }

    // Rule 1: build DAG edges (i < j, writeSet conflict)
    let mut blockers: Vec<Vec<String>> = vec![Vec::new(); tasks.len()];
    for (i, ti) in tasks.iter().enumerate() {
        for (j, tj) in tasks.iter().enumerate() {
            if i < j && crate::preset::write_sets_overlap_glob(&ti.write_set, &tj.write_set) {
                blockers[j].push(format!("task:{}", ti.id));
            }
            if i < j && crate::preset::write_sets_overlap_glob(&tj.write_set, &ti.write_set) {
                // symmetric: also j→i if j's set conflicts with i's
                // Actually write_sets_overlap_glob is symmetric, so the above covers both directions.
                // We only need i→j edges for i < j.
            }
        }
    }

    // Rule 2 & 5: produce actions in input order
    let mut actions = Vec::new();
    let mut reserved_agents = snapshot.active_agent_counts.clone();
    let mut reserved_quotas = snapshot.active_quota_counts.clone();

    for (i, t) in tasks.iter().enumerate() {
        // Rule 2: Recorded tasks skipped
        if snapshot.recorded.contains(&t.id) {
            continue;
        }

        // Rule 2: check DAG blockers — all must be Recorded
        let pending_blockers: Vec<String> = blockers[i]
            .iter()
            .filter(|b| {
                let blocker_id = b.strip_prefix("task:").unwrap_or(b);
                !snapshot.recorded.contains(blocker_id)
            })
            .cloned()
            .collect();

        if !pending_blockers.is_empty() {
            actions.push(SchedulerAction::Wait {
                task: t.id.clone(),
                blockers: pending_blockers,
            });
            continue;
        }

        // Rule 3 & 4: check agent/quota capacity and reserve immediately
        let agent_capacity = limits.agents[&t.agent];
        let quota_capacity = limits.quota_domains[&t.quota_domain];
        let active_agent = reserved_agents.get(&t.agent).copied().unwrap_or(0);
        let active_quota = reserved_quotas.get(&t.quota_domain).copied().unwrap_or(0);
        if active_agent >= agent_capacity || active_quota >= quota_capacity {
            actions.push(SchedulerAction::Wait {
                task: t.id.clone(),
                blockers: Vec::new(),
            });
            continue;
        }

        // Rule 4: immediate reservation
        *reserved_agents.entry(t.agent.clone()).or_insert(0) += 1;
        *reserved_quotas.entry(t.quota_domain.clone()).or_insert(0) += 1;
        actions.push(SchedulerAction::Dispatch(t.id.clone()));
    }

    Ok(actions)
}

// ─────────────────────────── B135：升级阶梯与审查链（纯函数内核） ───────────────────────────

/// Derive an escalation suffix from the signed hierarchy order.
///
/// The input order is authoritative: candidates are never reordered and the
/// suffix never wraps around. Unknown agents fail closed.
pub fn escalation_chain_from_hierarchy(
    hierarchy: &[String],
    agent: &str,
) -> Result<Vec<String>, String> {
    let start = hierarchy
        .iter()
        .position(|known| known == agent)
        .ok_or_else(|| format!("unknown agent has no escalation chain: {agent}"))?;
    Ok(hierarchy[start..].to_vec())
}

/// Read the hierarchy from the active, signed ROUND-IR.
pub fn escalation_chain_from_round_ir(
    root: &std::path::Path,
    round: &str,
    agent: &str,
) -> Result<Vec<String>, String> {
    let ir = crate::plan::load_round_ir(root, round).map_err(|error| error.to_string())?;
    escalation_chain_from_hierarchy(&ir.scheduling.allowed_agents, agent)
}

/// Compatibility wrapper for callers that have not yet been lifted to an
/// explicit ROUND-IR context. New runtime decisions must use
/// [`escalation_chain_from_round_ir`] or [`escalation_chain_from_hierarchy`].
pub fn escalation_chain(agent: &str) -> Result<Vec<String>, String> {
    escalation_chain_from_hierarchy(
        &[
            "executor-desktop".to_string(),
            "executor-claw".to_string(),
            "executor-opencode".to_string(),
        ],
        agent,
    )
}

/// 链序第一个既不在 failed（两振出局名单）也不在 busy 的 agent。
/// 严格按链序扫描——**不因空闲越级**（绝不重排序）；链耗尽 → Err
/// （语义 = root takeover，绝不回绕到链首）。
pub fn next_candidate(
    chain: &[String],
    failed: &[String],
    busy: &[String],
) -> Result<String, String> {
    for agent in chain {
        if !failed.iter().any(|f| f == agent) && !busy.iter().any(|b| b == agent) {
            return Ok(agent.clone());
        }
    }
    Err("escalation chain exhausted: root takeover (never wrap around)".to_string())
}

// ─────────────────────────── B147：接替资格全校验（纯函数内核） ───────────────────────────

/// 自动接替的资格闸：候选 agent 的 roles 必须**覆盖**任务声明的
/// `required_caps`（caps ⊆ roles）；缺失时 Err 点名缺哪项——**绝不降级**
/// 把 critical 任务交给无 critical 能力的候选。空需求恒满足。
pub fn successor_eligible(
    required_caps: &[String],
    candidate_roles: &[String],
) -> Result<(), String> {
    let missing: Vec<&String> = required_caps
        .iter()
        .filter(|capability| !candidate_roles.iter().any(|role| role == *capability))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "successor 缺必需能力（拒绝降级接替）: {}",
            missing
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// 签核序第一个通过**全部**过滤的候选：不在 failed（两振出局）、不在
/// ineligible（能力不合格）、不在 busy。严格按链序扫描——绝不重排序、
/// 绝不回绕、绝不降级；链耗尽 → Err 含 `eligible-chain-exhausted`
/// （语义 = 交 planner 升级，绝不静默选个不合格候选）。
pub fn next_eligible(
    chain: &[String],
    failed: &[String],
    ineligible: &[String],
    busy: &[String],
) -> Result<String, String> {
    for agent in chain {
        if !failed.iter().any(|f| f == agent)
            && !ineligible.iter().any(|i| i == agent)
            && !busy.iter().any(|b| b == agent)
        {
            return Ok(agent.clone());
        }
    }
    Err("eligible-chain-exhausted: root takeover (never wrap around, never downgrade)".to_string())
}

/// 实现者的审查链（角色, agent）序列。**实现者绝不进自己的审查链；
/// executor-desktop 绝不担任审查角色**（用户层级裁定）；未知实现者 → Err。
pub fn review_chain(implementer: &str) -> Result<Vec<(String, String)>, String> {
    let chain: Vec<(&str, &str)> = match implementer {
        "executor-desktop" => {
            vec![
                ("primary", "executor-claw"),
                ("secondary", "executor-opencode"),
            ]
        }
        "executor-claw" => vec![("primary", "executor-opencode")],
        "executor-opencode" => vec![("primary", "executor-claw")],
        other => return Err(format!("unknown implementer has no review chain: {other}")),
    };
    Ok(chain
        .into_iter()
        .map(|(role, agent)| (role.to_string(), agent.to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, agent: &str, quota: &str, write_set: &[&str]) -> SchedulerTask {
        SchedulerTask {
            id: id.into(),
            agent: agent.into(),
            quota_domain: quota.into(),
            write_set: write_set.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn snap(recorded: &[&str]) -> SchedulerSnapshot {
        SchedulerSnapshot {
            recorded: recorded.iter().map(|s| s.to_string()).collect(),
            active_agents: HashSet::new(),
            active_quota_domains: HashSet::new(),
        }
    }

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
        let unrelated =
            b246_review_request("B999", "B999-A0001", "primary", "executor-opencode");
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
    fn ab_independent_c_only_conflicts_a() {
        // A/B 独立（writeSet 不冲突）；C 只冲突 A。
        // A Recorded 后即可派 C，不等待 B。
        let tasks = vec![
            task("A", "codex", "openai", &["src/a.rs"]),
            task("B", "opencode", "zhipu", &["src/b.rs"]),
            task("C", "claw", "moonshot", &["src/a.rs"]),
        ];
        // 初始：A/B 可派（不同 lane），C 被 A 阻塞
        let first = plan_actions(&tasks, &snap(&[])).unwrap();
        assert_eq!(
            first,
            vec![
                SchedulerAction::Dispatch("A".into()),
                SchedulerAction::Dispatch("B".into()),
                SchedulerAction::Wait {
                    task: "C".into(),
                    blockers: vec!["task:A".into()],
                },
            ]
        );

        // A Recorded 后：C 不再被阻塞（B 独立，不影响 C）
        let second = plan_actions(&tasks, &snap(&["A"])).unwrap();
        assert_eq!(
            second,
            vec![
                SchedulerAction::Dispatch("B".into()),
                SchedulerAction::Dispatch("C".into()),
            ]
        );
    }

    #[test]
    fn counted_quota_capacity_is_not_boolean() {
        let tasks = vec![
            task("A", "agent-a", "shared", &["src/a.rs"]),
            task("B", "agent-b", "shared", &["src/b.rs"]),
            task("C", "agent-c", "shared", &["src/c.rs"]),
        ];
        let limits = CapacityLimits {
            agents: BTreeMap::from([
                ("agent-a".into(), 1),
                ("agent-b".into(), 1),
                ("agent-c".into(), 1),
            ]),
            quota_domains: BTreeMap::from([("shared".into(), 2)]),
        };
        let actions = plan_actions_with_capacity(
            &tasks,
            &CapacitySnapshot {
                recorded: BTreeSet::new(),
                active_agent_counts: BTreeMap::new(),
                active_quota_counts: BTreeMap::new(),
            },
            &limits,
        )
        .unwrap();
        assert!(matches!(actions[0], SchedulerAction::Dispatch(_)));
        assert!(matches!(actions[1], SchedulerAction::Dispatch(_)));
        assert!(matches!(actions[2], SchedulerAction::Wait { .. }));
    }

    #[test]
    fn unknown_and_zero_capacity_errors_name_task_and_lane() {
        let tasks = vec![task("A", "agent-a", "quota-a", &["src/a.rs"])];
        let snapshot = CapacitySnapshot {
            recorded: BTreeSet::new(),
            active_agent_counts: BTreeMap::new(),
            active_quota_counts: BTreeMap::new(),
        };
        let unknown = plan_actions_with_capacity(
            &tasks,
            &snapshot,
            &CapacityLimits {
                agents: BTreeMap::new(),
                quota_domains: BTreeMap::from([("quota-a".into(), 1)]),
            },
        )
        .unwrap_err();
        assert!(unknown.contains("A") && unknown.contains("agent-a"));

        let zero = plan_actions_with_capacity(
            &tasks,
            &snapshot,
            &CapacityLimits {
                agents: BTreeMap::from([("agent-a".into(), 1)]),
                quota_domains: BTreeMap::from([("quota-a".into(), 0)]),
            },
        )
        .unwrap_err();
        assert!(zero.contains("A") && zero.contains("quota-a") && zero.contains('0'));
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
