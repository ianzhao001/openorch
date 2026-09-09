//! Non-frozen B239 regression coverage for the concurrency and timing gaps intentionally left out
//! of the relocated contract seed.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use orch_host::gate::{
    orphan_baseline, orphan_failure_event, orphans_since, partition_gate_orphans,
    register_gate_fixture,
};
use orch_host::util::test_scratch_dir;

const WRITER_PATH: &str = "ORCH_B239_WRITER_PATH";
const WRITER_PID: &str = "ORCH_B239_WRITER_PID";
const WRITER_PGID: &str = "ORCH_B239_WRITER_PGID";

extern "C" {
    fn getpgrp() -> i32;
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}

struct KillGroupOnDrop(i32);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        // SAFETY: the test owns this positive, independently-created process group.
        unsafe {
            libc_kill(-self.0, 9);
        }
    }
}

#[test]
fn b239_registry_writer_child() {
    let Some(path) = std::env::var_os(WRITER_PATH) else {
        return;
    };
    let pid = std::env::var(WRITER_PID)
        .expect("writer pid env")
        .parse::<u32>()
        .expect("writer pid integer");
    let pgid = std::env::var(WRITER_PGID)
        .expect("writer pgid env")
        .parse::<u32>()
        .expect("writer pgid integer");
    register_gate_fixture(Some(path.as_ref()), pid, pgid)
        .expect("concurrent child registration")
        .expect("writer path is present");
}

#[test]
fn concurrent_process_registrations_keep_every_complete_line() {
    const WRITERS: usize = 12;
    const ROUNDS: usize = 3;
    let root = test_scratch_dir("b239-registry-concurrency");
    let executable = std::env::current_exe().expect("resolve integration test executable");

    for round in 0..ROUNDS {
        let registry = root.join(format!("round-{round}.fixtures"));
        let mut children = Vec::new();
        let expected = (0..WRITERS)
            .map(|index| 700_000u32 + (round * WRITERS + index) as u32)
            .collect::<BTreeSet<_>>();
        for pid in &expected {
            let child = Command::new(&executable)
                .args([
                    "--exact",
                    "b239_registry_writer_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(WRITER_PATH, &registry)
                .env(WRITER_PID, pid.to_string())
                .env(WRITER_PGID, pid.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn concurrent registry writer");
            children.push(child);
        }
        for mut child in children {
            assert!(child.wait().expect("wait registry writer").success());
        }

        let bytes = fs::read(&registry).expect("read concurrent registry");
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "registry must end on a complete line"
        );
        let text = String::from_utf8(bytes).expect("registry must be UTF-8");
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), WRITERS, "round {round} lost a registration");
        let observed = lines
            .iter()
            .map(|line| {
                let fields = line.split('\t').collect::<Vec<_>>();
                assert_eq!(fields.len(), 3, "round {round} has a torn line: {line:?}");
                assert_eq!(fields[0], fields[1], "fixture pid/pgid fixture mismatch");
                fields[1].parse::<u32>().expect("registered pgid integer")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            observed, expected,
            "round {round} lost or duplicated a writer"
        );
        println!(
            "B239_CONCURRENT_REGISTRY round={} writers={} lines={} complete=true pids={:?}",
            round + 1,
            WRITERS,
            lines.len(),
            observed
        );
    }

    fs::remove_dir_all(root).expect("remove concurrency scratch root");
}

#[test]
fn per_pid_registration_startup_cost_is_measured() {
    const SAMPLES: usize = 9;
    let root = test_scratch_dir("b239-registration-timing");
    let registry = root.join("timing.fixtures");
    let pid = std::process::id();
    // SAFETY: getpgrp has no preconditions and reads this process's actual group.
    let pgid = unsafe { getpgrp() };
    assert!(pgid > 0);

    let mut baseline = Vec::new();
    let mut registered = Vec::new();
    for _ in 0..SAMPLES {
        let start = Instant::now();
        register_gate_fixture(None, pid, pgid as u32).expect("no-op timing sample");
        baseline.push(start.elapsed());

        let start = Instant::now();
        register_gate_fixture(Some(registry.as_os_str()), pid, pgid as u32)
            .expect("registered timing sample");
        registered.push(start.elapsed());
    }
    baseline.sort_unstable();
    registered.sort_unstable();
    let baseline_median = baseline[SAMPLES / 2];
    let registered_median = registered[SAMPLES / 2];
    let registered_max = *registered.last().expect("registered samples");
    println!(
        "B239_REGISTRATION_TIMING samples={} baseline_median_us={} registered_median_us={} registered_max_us={} delta_median_us={}",
        SAMPLES,
        baseline_median.as_micros(),
        registered_median.as_micros(),
        registered_max.as_micros(),
        registered_median.saturating_sub(baseline_median).as_micros()
    );
    assert!(
        registered_max < Duration::from_millis(50),
        "per-PID registration unexpectedly resembles a full-table census: max={registered_max:?}"
    );
    fs::remove_dir_all(root).expect("remove timing scratch root");
}

#[test]
fn stubborn_unregistered_escape_is_detected_but_not_claimed_as_fixed() {
    let baseline = (0..10)
        .find_map(|_| match orphan_baseline() {
            Ok(value) => Some(value),
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                None
            }
        })
        .expect("sample stubborn-fixture baseline");
    let root = test_scratch_dir("b239-stubborn-detection");
    let pidfile = root.join("stubborn.pid");
    let script = format!(
        "(trap '' TERM; while :; do sleep 60; done) & printf '%s' \"$!\" > '{}'",
        pidfile.display()
    );
    let mut launcher = Command::new("/bin/sh")
        .args(["-c", script.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn stubborn escape launcher");
    let pgid = launcher.id() as i32;
    let _cleanup = KillGroupOnDrop(pgid);
    assert!(launcher.wait().expect("wait stubborn launcher").success());
    let escapee = fs::read_to_string(&pidfile)
        .expect("read stubborn pid")
        .parse::<u32>()
        .expect("stubborn pid integer");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = Vec::new();
    while Instant::now() < deadline {
        if let Ok(rows) = orphans_since(&baseline) {
            observed = rows;
            if observed.iter().any(|row| row.pid == escapee) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (reportable, evidence_only) = partition_gate_orphans(&observed);
    let hit = reportable
        .iter()
        .find(|row| row.pid == escapee)
        .unwrap_or_else(|| {
            panic!(
                "stubborn ppid=1 escape was not reportable: raw={observed:?} evidence_only={evidence_only:?}"
            )
        });
    let event = orphan_failure_event("B239", "r67", std::slice::from_ref(hit));
    let reason = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("reason"))
        .and_then(serde_json::Value::as_str)
        .expect("stubborn detection event reason");
    assert!(reason.contains(&escapee.to_string()));
    assert!(reason.contains(&hit.executable));
    println!(
        "B239_STUBBORN_DETECTION pid={} pgid={} raw={:?} reportable={:?} root_cause_fixed=false",
        escapee, pgid, observed, reportable
    );

    // This card detects the unregistered form but intentionally does not repair its in-gate hang.
    // The test owns the synthetic group and removes it explicitly after recording the evidence.
    // SAFETY: the test owns this positive, independently-created process group.
    unsafe {
        libc_kill(-pgid, 9);
    }
    fs::remove_dir_all(root).expect("remove stubborn detection scratch root");
}

/// Explicit five-minute idle-window evidence. Kept ignored so ordinary fast gates do not acquire a
/// five-minute floor; B239 executes it once with `--ignored --nocapture` during delivery.
#[test]
#[ignore = "B239 explicit five-minute idle-window evidence"]
fn consumer_filter_is_quiet_during_an_idle_window() {
    let seconds = std::env::var("ORCH_B239_IDLE_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(300);
    let baseline = orphan_baseline().expect("sample idle-window baseline");
    std::thread::sleep(Duration::from_secs(seconds));
    let raw = orphans_since(&baseline).expect("sample idle-window raw diff");
    let (reportable, evidence_only) = partition_gate_orphans(&raw);
    println!(
        "B239_IDLE_WINDOW seconds={seconds} raw={raw:?} reportable={reportable:?} evidence_only={evidence_only:?}"
    );
    assert!(
        reportable.is_empty(),
        "idle window produced gate-workset orphans; the window was not idle: {reportable:?}"
    );
}
