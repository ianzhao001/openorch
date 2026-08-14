//! B249 contract seed: successful process-group KILL is not proof of convergence.
//!
//! Expected red: compile (`E0432`).  The current host exposes the legacy one-shot
//! `reap_gate_process_group`, but not the injectable convergence control surface,
//! observations, or fixed-budget constants imported below.
//!
//! This file is frozen after relocation.  Supplementary tests belong in `gate.rs`
//! or `exclusive_lane_contract.rs`; never edit this seed to make an implementation pass.
//!
//! Negative mutations required by the task card:
//! - M1: return success immediately after the first successful KILL;
//! - M2: re-signal after identity drift or an ambiguous observation;
//! - M3: reset the total deadline after each retry, or omit survivor diagnostics;
//! - M4: bypass the shared kernel from either the direct or registry production path.

use std::collections::VecDeque;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use orch_host::gate::b249_contract::{
    reap_gate_process_group, reap_gate_process_group_with_control, GateGroupMember,
    GateGroupObservation, GateReapControl, GATE_REAP_POLL_INTERVAL, GATE_REAP_TOTAL_BUDGET,
};

#[derive(Debug)]
struct FakeControl {
    observations: VecDeque<Result<GateGroupObservation, String>>,
    last_observation: Option<Result<GateGroupObservation, String>>,
    signal_results: VecDeque<std::io::Result<()>>,
    elapsed: Duration,
    signalled: Vec<u32>,
    sleeps: Vec<Duration>,
}

impl FakeControl {
    fn new(observations: Vec<GateGroupObservation>) -> Self {
        Self {
            observations: observations.into_iter().map(Ok).collect(),
            last_observation: None,
            signal_results: VecDeque::new(),
            elapsed: Duration::ZERO,
            signalled: Vec::new(),
            sleeps: Vec::new(),
        }
    }

    fn with_repeating_last(mut self) -> Self {
        self.last_observation = self.observations.back().cloned();
        self
    }
}

impl GateReapControl for FakeControl {
    fn observe_group(&mut self, _pgid: u32) -> Result<GateGroupObservation, String> {
        if let Some(observation) = self.observations.pop_front() {
            self.last_observation = Some(observation.clone());
            return observation;
        }
        self.last_observation
            .clone()
            .unwrap_or_else(|| Ok(GateGroupObservation::Gone))
    }

    fn signal_kill(&mut self, pgid: u32) -> std::io::Result<()> {
        self.signalled.push(pgid);
        self.signal_results.pop_front().unwrap_or(Ok(()))
    }

    fn elapsed(&self) -> Duration {
        self.elapsed
    }

    fn sleep(&mut self, duration: Duration) {
        self.sleeps.push(duration);
        self.elapsed = self.elapsed.saturating_add(duration);
    }
}

fn member(pid: u32, birth: &str) -> GateGroupMember {
    GateGroupMember::live(pid, birth)
}

#[test]
fn a_successful_kill_rechecks_and_rekills_a_leaderless_residual_group() {
    let mut control = FakeControl::new(vec![
        GateGroupObservation::Members(vec![member(4100, "leader-A")]),
        // The reproduced failure is harsher than a surviving leader: the shell/leader
        // is already dead and only a child forked into the old PGID remains.  A PGID
        // cannot be reused while that residual group still has a member, so this exact
        // leaderless shape remains safe to re-signal within the original deadline.
        GateGroupObservation::Members(vec![member(4101, "late-child-B")]),
        GateGroupObservation::Gone,
    ]);

    reap_gate_process_group_with_control(4100, None, &mut control)
        .expect("leaderless old-PGID late child should converge");

    assert_eq!(control.signalled, vec![4100, 4100]);
    assert_eq!(GATE_REAP_TOTAL_BUDGET, Duration::from_secs(2));
    assert_eq!(GATE_REAP_POLL_INTERVAL, Duration::from_millis(10));
}

#[test]
fn a_direct_initial_leaderless_residual_group_is_still_reaped() {
    let mut control = FakeControl::new(vec![
        GateGroupObservation::Members(vec![member(4151, "residual-child")]),
        GateGroupObservation::Gone,
    ]);

    reap_gate_process_group_with_control(4150, None, &mut control)
        .expect("direct normal-exit cleanup must reap a leaderless residual group");

    assert_eq!(control.signalled, vec![4150]);
}

#[test]
fn a_registry_anchor_can_reap_an_initial_leaderless_residual_group() {
    let mut control = FakeControl::new(vec![
        GateGroupObservation::Members(vec![member(4176, "registered-residual-child")]),
        GateGroupObservation::Gone,
    ]);

    reap_gate_process_group_with_control(4175, Some("recorded-leader"), &mut control)
        .expect("recorded registry epoch plus a residual member must authorize cleanup");

    assert_eq!(control.signalled, vec![4175]);
}

#[test]
fn identity_drift_refuses_a_second_signal() {
    let mut control = FakeControl::new(vec![
        GateGroupObservation::Members(vec![member(4200, "leader-old")]),
        GateGroupObservation::Members(vec![member(4200, "leader-reused")]),
    ]);

    let error = reap_gate_process_group_with_control(4200, None, &mut control)
        .expect_err("identity drift must fail closed")
        .to_string();

    assert_eq!(control.signalled, vec![4200]);
    assert!(error.contains("4200"), "diagnostic must name pgid: {error}");
    assert!(
        error.contains("identity") || error.contains("birth"),
        "diagnostic must name the identity mismatch: {error}"
    );
}

#[test]
fn a_recorded_anchor_mismatch_refuses_the_first_signal() {
    let mut control = FakeControl::new(vec![GateGroupObservation::Members(vec![member(
        4250,
        "leader-observed",
    )])]);

    let error = reap_gate_process_group_with_control(4250, Some("leader-recorded"), &mut control)
        .expect_err("registry anchor mismatch must fail before signalling")
        .to_string();

    assert!(
        control.signalled.is_empty(),
        "mismatched anchor authorized KILL"
    );
    assert!(error.contains("4250"), "diagnostic must name pgid: {error}");
    assert!(
        error.contains("anchor") || error.contains("identity") || error.contains("birth"),
        "diagnostic must name the anchor mismatch: {error}"
    );
}

#[test]
fn an_ambiguous_initial_census_refuses_the_first_signal() {
    let mut control = FakeControl::new(vec![GateGroupObservation::Ambiguous {
        reason: "initial member birth unavailable".to_string(),
    }]);

    let error = reap_gate_process_group_with_control(4275, None, &mut control)
        .expect_err("initial ambiguity must fail before signalling")
        .to_string();

    assert!(
        control.signalled.is_empty(),
        "ambiguous initial census authorized KILL"
    );
    assert!(error.contains("ambiguous"), "actual diagnostic: {error}");
}

#[test]
fn an_ambiguous_census_never_authorizes_another_kill() {
    let mut control = FakeControl::new(vec![
        GateGroupObservation::Members(vec![member(4300, "leader-A")]),
        GateGroupObservation::Ambiguous {
            reason: "member birth unavailable".to_string(),
        },
    ]);

    let error = reap_gate_process_group_with_control(4300, None, &mut control)
        .expect_err("ambiguous observation must fail closed")
        .to_string();

    assert_eq!(control.signalled, vec![4300]);
    assert!(error.contains("ambiguous"), "actual diagnostic: {error}");
}

#[test]
fn the_total_budget_never_resets_and_timeout_names_survivors() {
    let survivor = GateGroupObservation::Members(vec![member(4400, "leader-A")]);
    let mut control = FakeControl::new(vec![survivor.clone(), survivor]).with_repeating_last();

    let error = reap_gate_process_group_with_control(4400, Some("leader-A"), &mut control)
        .expect_err("a permanent survivor must time out")
        .to_string();

    assert!(
        control.elapsed >= GATE_REAP_TOTAL_BUDGET,
        "permanent survivors failed before consuming the fixed budget: elapsed={:?}",
        control.elapsed
    );
    assert!(
        control.elapsed <= GATE_REAP_TOTAL_BUDGET + GATE_REAP_POLL_INTERVAL,
        "retry reset the total deadline: elapsed={:?}",
        control.elapsed
    );
    assert!(
        control
            .sleeps
            .iter()
            .all(|duration| *duration <= GATE_REAP_POLL_INTERVAL),
        "a convergence poll slept beyond the fixed interval: {:?}",
        control.sleeps
    );
    assert!(error.contains("4400"), "missing pgid: {error}");
    assert!(error.contains("attempt"), "missing attempt count: {error}");
    assert!(
        error.contains("leader-A"),
        "missing survivor birth: {error}"
    );
    assert!(
        error.contains("zombie=false") || error.contains("live"),
        "missing survivor liveness: {error}"
    );
    assert!(
        control.signalled.len() >= 2,
        "survivors in the still-existing old PGID must receive bounded re-KILL attempts"
    );
}

#[test]
fn the_legacy_wrapper_remains_a_real_production_entrypoint() {
    let signature: fn(u32) -> anyhow::Result<()> = reap_gate_process_group;
    let _ = signature;
}

struct GroupGuard {
    pgid: u32,
    child: Option<Child>,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.pgid)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait();
        }
    }
}

fn group_is_alive(pgid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
#[ignore = "testExclusive:successful_kill_converges_under_pressure"]
fn successful_kill_converges_under_pressure() {
    for iteration in 0..8 {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30 & sleep 30 & wait")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let child = command.spawn().expect("spawn isolated fixture group");
        let pgid = child.id();
        let mut guard = GroupGuard {
            pgid,
            child: Some(child),
        };

        reap_gate_process_group(pgid).expect("production wrapper must converge");
        let deadline = Instant::now() + Duration::from_secs(3);
        while group_is_alive(pgid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !group_is_alive(pgid),
            "iteration {iteration}: successful reap returned with group {pgid} still alive"
        );
        if let Some(child) = guard.child.as_mut() {
            let _ = child.wait();
        }
        guard.child = None;
    }
}
