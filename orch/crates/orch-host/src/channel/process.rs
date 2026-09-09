//! Process-group termination primitives shared with selfhost reassignment.
//! Callers must establish ownership before invoking this kernel. Managed wake
//! transport additionally uses exact process credentials before granting signals.

#[cfg(feature = "selfhost")]
use std::time::{Duration, Instant};
#[cfg(feature = "selfhost")]
use anyhow::{bail, Context, Result};


/// Signals used by the bounded termination protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationSignal {
    /// Ask the owned group to terminate.
    Term,
    /// Escalate only after the caller and kernel authorize forced termination.
    Kill,
}

#[cfg(feature = "selfhost")]
mod reassignment {
use super::*;
/// Signals sent and the final group-liveness observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminationEvidence {
    /// Ordered signals actually sent by the kernel.
    pub signals: Vec<TerminationSignal>,
    /// Whether the final observation established group absence.
    pub process_tree_terminated: bool,
}

/// Require both captured termination and a fresh negative liveness observation.
pub fn termination_commit_gate(
    evidence_terminated: bool,
    recheck_group_alive: Option<bool>,
) -> Result<(), String> {
    if evidence_terminated && recheck_group_alive == Some(false) {
        Ok(())
    } else {
        Err(format!(
            "termination commit blocked: evidenceTerminated={evidence_terminated}, recheckGroupAlive={recheck_group_alive:?}"
        ))
    }
}

/// Send one explicit signal to the already-authorized nonzero process group.
fn signal_process_group(pgid: u32, signal: &str) -> Result<bool> {
    if pgid == 0 {
        bail!("process-group id must be non-zero");
    }
    let target = format!("-{pgid}");
    let status = std::process::Command::new("/bin/kill")
        .args([signal, "--", &target])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("invoke /bin/kill {signal} for process group {pgid}"))?;
    Ok(status.success())
}

/// Observe an already-identified process group using signal zero.
pub(crate) fn process_group_alive(pgid: u32) -> Result<bool> {
    signal_process_group(pgid, "-0")
}

/// Bounded timing policy for managed process-group termination.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedTerminationPolicy {
    /// Grace allowed after TERM before considering escalation.
    pub term_grace: Duration,
    /// Maximum verification interval after KILL.
    pub kill_verify: Duration,
    /// Interval between liveness observations.
    pub poll_interval: Duration,
}

/// Deterministic control surface for the managed process-group termination
/// kernel.  Production delegates to exact group signals and monotonic time;
/// tests can exercise convergence without wall-clock sleeps.
#[doc(hidden)]
pub trait ManagedProcessGroupControl {
    fn monotonic_now(&mut self) -> Duration;
    fn group_alive(&mut self, pgid: u32) -> std::result::Result<bool, String>;
    fn signal_group(
        &mut self,
        pgid: u32,
        signal: TerminationSignal,
    ) -> std::result::Result<bool, String>;
    fn sleep(&mut self, duration: Duration);
}

/// Shared fail-closed kernel for exact process-group termination.
///
/// Physical KILL delivery may repeat inside one fixed verification window so
/// a same-PGID member forked during the first sweep cannot escape.  The public
/// evidence intentionally records the logical TERM/KILL escalation only once.
#[doc(hidden)]
pub fn terminate_managed_process_group_with_control<C: ManagedProcessGroupControl>(
    pgid: u32,
    policy: ManagedTerminationPolicy,
    control: &mut C,
) -> std::result::Result<TerminationEvidence, String> {
    if pgid == 0 {
        return Err("process-group id must be non-zero".into());
    }
    if pgid == std::process::id() {
        return Err(format!(
            "refusing to terminate orch's own pid/process-group identity {pgid}"
        ));
    }
    if policy.poll_interval.is_zero() {
        return Err("managed termination poll interval must be non-zero".into());
    }

    let mut last_now = control.monotonic_now();
    if !control.group_alive(pgid)? {
        return Ok(TerminationEvidence {
            signals: Vec::new(),
            process_tree_terminated: true,
        });
    }

    let mut signals = vec![TerminationSignal::Term];
    let _signal_status = control.signal_group(pgid, TerminationSignal::Term)?;
    let term_started = managed_termination_now(control, &mut last_now)?;
    let term_deadline =
        managed_termination_deadline(term_started, policy.term_grace, "TERM grace")?;
    loop {
        if !control.group_alive(pgid)? {
            return Ok(TerminationEvidence {
                signals,
                process_tree_terminated: true,
            });
        }
        let now = managed_termination_now(control, &mut last_now)?;
        if now >= term_deadline {
            break;
        }
        control.sleep(policy.poll_interval.min(term_deadline - now));
    }

    // Compute the post-KILL deadline exactly once on phase entry.  Even a zero
    // verification budget still delivers the first escalation before probing.
    let kill_started = managed_termination_now(control, &mut last_now)?;
    let kill_deadline =
        managed_termination_deadline(kill_started, policy.kill_verify, "KILL verification")?;
    signals.push(TerminationSignal::Kill);
    let _signal_status = control.signal_group(pgid, TerminationSignal::Kill)?;

    loop {
        // Signal command success is never death evidence.  Only this fresh
        // post-signal probe can establish that the exact group is absent.
        if !control.group_alive(pgid)? {
            return Ok(TerminationEvidence {
                signals,
                process_tree_terminated: true,
            });
        }
        let now = managed_termination_now(control, &mut last_now)?;
        if now >= kill_deadline {
            return Ok(TerminationEvidence {
                signals,
                process_tree_terminated: false,
            });
        }

        // Re-sweep the whole PGID before every bounded poll.  Repeated
        // physical delivery deliberately does not append another logical
        // TerminationSignal::Kill to the evidence vector.
        let _signal_status = control.signal_group(pgid, TerminationSignal::Kill)?;
        control.sleep(policy.poll_interval.min(kill_deadline - now));
    }
}

struct SystemManagedProcessGroupControl {
    origin: Instant,
}

impl SystemManagedProcessGroupControl {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl ManagedProcessGroupControl for SystemManagedProcessGroupControl {
    fn monotonic_now(&mut self) -> Duration {
        self.origin.elapsed()
    }

    fn group_alive(&mut self, pgid: u32) -> std::result::Result<bool, String> {
        process_group_alive(pgid).map_err(|error| format!("{error:#}"))
    }

    fn signal_group(
        &mut self,
        pgid: u32,
        signal: TerminationSignal,
    ) -> std::result::Result<bool, String> {
        let signal = match signal {
            TerminationSignal::Term => "-TERM",
            TerminationSignal::Kill => "-KILL",
        };
        signal_process_group(pgid, signal).map_err(|error| format!("{error:#}"))
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Terminate an exact runtime-owned process group and verify it with group
/// `kill -0`.  TERM is followed by the configured grace period, then KILL only
/// if the group remains.  The caller supplies the exact spawn pid/pgid from
/// attempt-scoped channel facts; refusing orch's own pid is a final local
/// corruption guard that does not depend on sandbox-restricted process listing.
pub fn terminate_managed_process_group(pgid: u32, grace: Duration) -> Result<TerminationEvidence> {
    let policy = ManagedTerminationPolicy {
        term_grace: grace,
        kill_verify: Duration::from_secs(1),
        poll_interval: Duration::from_millis(20),
    };
    let mut control = SystemManagedProcessGroupControl::new();
    terminate_managed_process_group_with_control(pgid, policy, &mut control)
        .map_err(anyhow::Error::msg)
}


fn managed_termination_now<C: ManagedProcessGroupControl>(
    control: &mut C,
    previous: &mut Duration,
) -> std::result::Result<Duration, String> {
    let now = control.monotonic_now();
    if now < *previous {
        return Err(format!(
            "managed termination monotonic clock moved backwards: previous={previous:?}, now={now:?}"
        ));
    }
    *previous = now;
    Ok(now)
}

fn managed_termination_deadline(
    start: Duration,
    budget: Duration,
    phase: &str,
) -> std::result::Result<Duration, String> {
    start
        .checked_add(budget)
        .ok_or_else(|| format!("managed termination {phase} deadline overflow"))
}

}
#[cfg(feature = "selfhost")]
pub use reassignment::*;
#[cfg(feature = "selfhost")]
pub(crate) use reassignment::process_group_alive;
