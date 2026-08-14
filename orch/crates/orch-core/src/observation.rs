//! Metadata-only module observations and their deterministic health projection.
//!
//! This module deliberately owns no collectors, storage, clocks, or control-plane
//! actions. Callers provide both the evidence set and the snapshot time.

use std::collections::{BTreeMap, BTreeSet};

/// The library fallback is intentionally between the contract's fresh and stale
/// fixture ages. A later integration may supply signed policy values elsewhere.
const DEFAULT_FRESHNESS_WINDOW_SECS: i64 = 300;

/// The typed source represented by an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationSource {
    Heartbeat,
    Activity,
    Terminal,
}

/// A typed terminal result. It describes transport/execution evidence, not task
/// quality or a verifier verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TerminalOutcome {
    Ok,
    Failed,
}

/// A minimal, metadata-only observation contract.
///
/// Fields are private so the constructors remain the single place that maintains
/// the relationship between `source` and `terminal_outcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleObservationV0 {
    observation_id: String,
    agent_id: String,
    attempt_id: Option<String>,
    observed_at_epoch_s: i64,
    source: ObservationSource,
    terminal_outcome: Option<TerminalOutcome>,
    inflight: Option<bool>,
}

impl ModuleObservationV0 {
    pub fn heartbeat(
        observation_id: impl Into<String>,
        agent_id: impl Into<String>,
        observed_at_epoch_s: i64,
    ) -> Self {
        Self::new(
            observation_id,
            agent_id,
            observed_at_epoch_s,
            ObservationSource::Heartbeat,
            None,
        )
    }

    pub fn activity(
        observation_id: impl Into<String>,
        agent_id: impl Into<String>,
        observed_at_epoch_s: i64,
    ) -> Self {
        Self::new(
            observation_id,
            agent_id,
            observed_at_epoch_s,
            ObservationSource::Activity,
            None,
        )
    }

    pub fn terminal(
        observation_id: impl Into<String>,
        agent_id: impl Into<String>,
        observed_at_epoch_s: i64,
        outcome: TerminalOutcome,
    ) -> Self {
        Self::new(
            observation_id,
            agent_id,
            observed_at_epoch_s,
            ObservationSource::Terminal,
            Some(outcome),
        )
    }

    fn new(
        observation_id: impl Into<String>,
        agent_id: impl Into<String>,
        observed_at_epoch_s: i64,
        source: ObservationSource,
        terminal_outcome: Option<TerminalOutcome>,
    ) -> Self {
        Self {
            observation_id: observation_id.into(),
            agent_id: agent_id.into(),
            attempt_id: None,
            observed_at_epoch_s,
            source,
            terminal_outcome,
            inflight: None,
        }
    }

    /// Bind the observation to an exact attempt identity.
    #[must_use]
    pub fn bound_exact(mut self, attempt_id: impl Into<String>) -> Self {
        self.attempt_id = Some(attempt_id.into());
        self
    }

    /// Attach the producer's explicit in-flight state.
    #[must_use]
    pub fn inflight(mut self, inflight: bool) -> Self {
        self.inflight = Some(inflight);
        self
    }

    pub fn observation_id(&self) -> &str {
        &self.observation_id
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn attempt_id(&self) -> Option<&str> {
        self.attempt_id.as_deref()
    }

    pub fn observed_at_epoch_s(&self) -> i64 {
        self.observed_at_epoch_s
    }

    pub fn source(&self) -> ObservationSource {
        self.source
    }

    pub fn terminal_outcome(&self) -> Option<TerminalOutcome> {
        self.terminal_outcome
    }

    pub fn is_inflight(&self) -> Option<bool> {
        self.inflight
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalState {
    Running,
    Idle,
    Disabled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthLevel {
    Ok,
    Warn,
    Unknown,
}

/// Effective policy for the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthPolicy {
    freshness_window_s: i64,
    disabled_agents: BTreeSet<String>,
}

impl HealthPolicy {
    pub fn library_default() -> Self {
        Self {
            freshness_window_s: DEFAULT_FRESHNESS_WINDOW_SECS,
            disabled_agents: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn mark_disabled(mut self, agent_id: impl Into<String>) -> Self {
        self.disabled_agents.insert(agent_id.into());
        self
    }

    pub fn freshness_window_s(&self) -> i64 {
        self.freshness_window_s
    }

    fn is_disabled(&self, agent_id: &str) -> bool {
        self.disabled_agents.contains(agent_id)
    }
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self::library_default()
    }
}

/// Orthogonal operational, health, freshness, and coverage projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentHealthV0 {
    operational_state: OperationalState,
    health_level: HealthLevel,
    is_stale: bool,
    unbound_evidence_count: usize,
}

impl AgentHealthV0 {
    fn new(
        operational_state: OperationalState,
        health_level: HealthLevel,
        is_stale: bool,
        unbound_evidence_count: usize,
    ) -> Self {
        Self {
            operational_state,
            health_level,
            is_stale,
            unbound_evidence_count,
        }
    }

    fn unknown(is_stale: bool, unbound_evidence_count: usize) -> Self {
        Self::new(
            OperationalState::Unknown,
            HealthLevel::Unknown,
            is_stale,
            unbound_evidence_count,
        )
    }

    pub fn operational_state(&self) -> OperationalState {
        self.operational_state
    }

    pub fn health_level(&self) -> HealthLevel {
        self.health_level
    }

    pub fn is_stale(&self) -> bool {
        self.is_stale
    }

    pub fn unbound_evidence_count(&self) -> usize {
        self.unbound_evidence_count
    }
}

/// Fold observations for one agent without reading external state.
///
/// Replay identity is resolved before health semantics. Identical payloads with
/// the same ID are a no-op; a semantic collision that touches this agent makes
/// its projection unknown. Unbound observations are counted but quarantined.
pub fn reduce_health(
    agent_id: &str,
    observations: &[ModuleObservationV0],
    now_epoch_s: i64,
    policy: &HealthPolicy,
) -> AgentHealthV0 {
    let unbound_evidence_count = observations
        .iter()
        .filter(|observation| {
            observation.agent_id() == agent_id && observation.attempt_id().is_none()
        })
        .map(ModuleObservationV0::observation_id)
        .collect::<BTreeSet<_>>()
        .len();

    let mut by_id: BTreeMap<&str, Vec<&ModuleObservationV0>> = BTreeMap::new();
    for observation in observations.iter().filter(|observation| {
        observation.agent_id() == agent_id && observation.attempt_id().is_some()
    }) {
        by_id
            .entry(observation.observation_id())
            .or_default()
            .push(observation);
    }

    let mut bound = Vec::new();
    let mut collision_affects_agent = false;

    for same_id in by_id.values() {
        let first = same_id[0];
        let collides = same_id.iter().skip(1).any(|item| *item != first);

        if collides {
            collision_affects_agent = true;
            continue;
        }

        bound.push(first);
    }

    if policy.is_disabled(agent_id) {
        return AgentHealthV0::new(
            OperationalState::Disabled,
            HealthLevel::Unknown,
            false,
            unbound_evidence_count,
        );
    }

    if collision_affects_agent {
        return AgentHealthV0::unknown(false, unbound_evidence_count);
    }

    let attempts: BTreeSet<&str> = bound
        .iter()
        .filter_map(|observation| observation.attempt_id())
        .collect();
    if attempts.len() != 1 {
        return AgentHealthV0::unknown(false, unbound_evidence_count);
    }

    let mut has_invalid_time = false;
    let mut has_stale = false;
    let mut fresh = Vec::new();
    for observation in &bound {
        match now_epoch_s.checked_sub(observation.observed_at_epoch_s()) {
            Some(age) if age >= 0 && age <= policy.freshness_window_s => {
                fresh.push(*observation);
            }
            Some(age) if age > policy.freshness_window_s => has_stale = true,
            _ => has_invalid_time = true,
        }
    }

    if has_invalid_time {
        return AgentHealthV0::unknown(false, unbound_evidence_count);
    }

    let terminals: Vec<&ModuleObservationV0> = bound
        .iter()
        .copied()
        .filter(|observation| observation.source() == ObservationSource::Terminal)
        .collect();
    if !terminals.is_empty() {
        let outcomes: BTreeSet<TerminalOutcome> = terminals
            .iter()
            .filter_map(|observation| observation.terminal_outcome())
            .collect();
        if outcomes.len() != 1 {
            return AgentHealthV0::unknown(false, unbound_evidence_count);
        }

        let has_fresh_terminal = terminals.iter().any(|terminal| {
            fresh
                .iter()
                .any(|observation| observation.observation_id() == terminal.observation_id())
        });
        if !has_fresh_terminal {
            return AgentHealthV0::unknown(true, unbound_evidence_count);
        }

        return match outcomes.iter().next().copied() {
            Some(TerminalOutcome::Ok) => AgentHealthV0::new(
                OperationalState::Idle,
                HealthLevel::Ok,
                false,
                unbound_evidence_count,
            ),
            Some(TerminalOutcome::Failed) => AgentHealthV0::new(
                OperationalState::Idle,
                HealthLevel::Warn,
                false,
                unbound_evidence_count,
            ),
            None => AgentHealthV0::unknown(false, unbound_evidence_count),
        };
    }

    if fresh.is_empty() {
        return AgentHealthV0::unknown(has_stale, unbound_evidence_count);
    }

    let latest_epoch_s = fresh
        .iter()
        .map(|observation| observation.observed_at_epoch_s())
        .max()
        .expect("fresh evidence is non-empty");
    let latest: Vec<&ModuleObservationV0> = fresh
        .into_iter()
        .filter(|observation| observation.observed_at_epoch_s() == latest_epoch_s)
        .collect();

    let running = latest.iter().any(|observation| {
        observation.source() == ObservationSource::Activity
            || observation.is_inflight() == Some(true)
    });
    let explicitly_idle = latest.iter().any(|observation| {
        observation.source() == ObservationSource::Heartbeat
            && observation.is_inflight() == Some(false)
    });
    if running && explicitly_idle {
        return AgentHealthV0::unknown(false, unbound_evidence_count);
    }
    if !running && !explicitly_idle {
        return AgentHealthV0::unknown(false, unbound_evidence_count);
    }

    AgentHealthV0::new(
        if running {
            OperationalState::Running
        } else {
            OperationalState::Idle
        },
        HealthLevel::Ok,
        false,
        unbound_evidence_count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 10_000;
    const AGENT: &str = "executor-example";
    const ATTEMPT: &str = "B1-A0001";

    #[test]
    fn semantic_id_collision_fails_closed() {
        let observations = vec![
            ModuleObservationV0::heartbeat("same", AGENT, NOW - 1)
                .bound_exact(ATTEMPT)
                .inflight(true),
            ModuleObservationV0::heartbeat("same", AGENT, NOW - 1)
                .bound_exact(ATTEMPT)
                .inflight(false),
        ];

        let health = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        assert_eq!(health.operational_state(), OperationalState::Unknown);
        assert_eq!(health.health_level(), HealthLevel::Unknown);
    }

    #[test]
    fn unbound_collision_is_quarantined_from_health() {
        let observations = vec![
            ModuleObservationV0::heartbeat("same", AGENT, NOW - 1)
                .bound_exact(ATTEMPT)
                .inflight(true),
            ModuleObservationV0::terminal("same", AGENT, NOW - 1, TerminalOutcome::Failed),
        ];

        let health = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        assert_eq!(health.operational_state(), OperationalState::Running);
        assert_eq!(health.health_level(), HealthLevel::Ok);
        assert_eq!(health.unbound_evidence_count(), 1);
    }

    #[test]
    fn terminal_precedes_non_terminal_evidence() {
        let observations = vec![
            ModuleObservationV0::heartbeat("hb", AGENT, NOW - 1)
                .bound_exact(ATTEMPT)
                .inflight(true),
            ModuleObservationV0::terminal("done", AGENT, NOW - 2, TerminalOutcome::Ok)
                .bound_exact(ATTEMPT),
        ];

        let health = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        assert_eq!(health.operational_state(), OperationalState::Idle);
        assert_eq!(health.health_level(), HealthLevel::Ok);
    }

    #[test]
    fn input_order_does_not_change_projection() {
        let mut observations = vec![
            ModuleObservationV0::heartbeat("heartbeat-old", AGENT, NOW - 2)
                .bound_exact(ATTEMPT)
                .inflight(true),
            ModuleObservationV0::heartbeat("heartbeat-new", AGENT, NOW - 1)
                .bound_exact(ATTEMPT)
                .inflight(false),
            ModuleObservationV0::terminal("orphan", AGENT, NOW - 1, TerminalOutcome::Failed),
        ];
        let forward = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        observations.reverse();
        let reversed = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());

        assert_eq!(forward, reversed);
        assert_eq!(forward.operational_state(), OperationalState::Idle);
        assert_eq!(forward.unbound_evidence_count(), 1);
    }

    #[test]
    fn multiple_exact_attempts_fail_closed() {
        let observations = vec![
            ModuleObservationV0::heartbeat("one", AGENT, NOW - 1)
                .bound_exact("B1-A0001")
                .inflight(true),
            ModuleObservationV0::heartbeat("two", AGENT, NOW - 1)
                .bound_exact("B1-A0002")
                .inflight(true),
        ];

        let health = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        assert_eq!(health.operational_state(), OperationalState::Unknown);
        assert_eq!(health.health_level(), HealthLevel::Unknown);
    }

    #[test]
    fn heartbeat_without_inflight_state_fails_closed() {
        let observations =
            vec![ModuleObservationV0::heartbeat("heartbeat", AGENT, NOW - 1).bound_exact(ATTEMPT)];

        let health = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        assert_eq!(health.operational_state(), OperationalState::Unknown);
        assert_eq!(health.health_level(), HealthLevel::Unknown);
    }

    #[test]
    fn failed_terminal_is_idle_and_warn() {
        let observations =
            vec![
                ModuleObservationV0::terminal("failed", AGENT, NOW - 1, TerminalOutcome::Failed)
                    .bound_exact(ATTEMPT),
            ];

        let health = reduce_health(AGENT, &observations, NOW, &HealthPolicy::default());
        assert_eq!(health.operational_state(), OperationalState::Idle);
        assert_eq!(health.health_level(), HealthLevel::Warn);
    }

    #[test]
    fn default_freshness_window_sits_between_frozen_fixture_ages() {
        let policy = HealthPolicy::library_default();
        assert!(policy.freshness_window_s() > 10);
        assert!(policy.freshness_window_s() < 7_200);
    }
}
