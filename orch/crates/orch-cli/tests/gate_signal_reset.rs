#![cfg(feature = "selfhost")]
//! B306 real-process regression: a gate child starts with default terminal dispositions even when
//! its foreground runtime parent inherited the ignored dispositions of a background shell job.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orch_host::binding::CommandSpec;

const HELPER_READY: &str = "ORCH_B306_GATE_SIGNAL_HELPER_READY";
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SIG_ERR: usize = usize::MAX;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn signal(signal: i32, handler: usize) -> usize;
}

struct SignalDispositionGuard {
    signal_number: i32,
    previous: usize,
}

impl SignalDispositionGuard {
    fn install(signal_number: i32, disposition: usize) -> Self {
        // SAFETY: the test binary has one test, installs one fixed POSIX disposition at a time,
        // and restores the exact previous value before moving to the next matrix cell.
        let previous = unsafe { signal(signal_number, disposition) };
        assert_ne!(previous, SIG_ERR, "failed to set signal {signal_number}");
        Self {
            signal_number,
            previous,
        }
    }
}

impl Drop for SignalDispositionGuard {
    fn drop(&mut self) {
        // SAFETY: `previous` is the handler value returned by signal(2) for this exact signal.
        let restored = unsafe { signal(self.signal_number, self.previous) };
        assert_ne!(
            restored, SIG_ERR,
            "failed to restore signal {}",
            self.signal_number
        );
    }
}

fn unique_temp(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("cli crate is below the orch workspace")
        .join("target/test-tmp")
        .join(format!(
            "b306-gate-signal-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
}

fn helper_process() {
    let ready = PathBuf::from(std::env::var_os(HELPER_READY).unwrap());
    fs::write(ready, format!("{}\n", std::process::id())).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn wait_and_signal(ready: &Path, signal_number: i32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !ready.exists() {
        return Err(format!(
            "gate child did not publish ready: {}",
            ready.display()
        ));
    }
    let pid = fs::read_to_string(ready)
        .map_err(|error| error.to_string())?
        .trim()
        .parse::<i32>()
        .map_err(|error| error.to_string())?;
    if pid <= 0 {
        return Err(format!("gate child published invalid pid {pid}"));
    }
    // SAFETY: run_gate creates a new process group whose positive leader PID is published by the
    // helper. Negating that exact value targets only the fixture group.
    if unsafe { kill(-pid, signal_number) } != 0 {
        return Err(format!(
            "signal {signal_number} failed for group {pid}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn run_cell(parent_disposition: usize, mode: &str, signal_number: i32) {
    let root = unique_temp(&format!("{mode}-{signal_number}"));
    let logs = root.join("logs");
    let ready = root.join("ready");
    fs::create_dir_all(&root).unwrap();
    let _parent_disposition = SignalDispositionGuard::install(signal_number, parent_disposition);
    std::env::set_var(HELPER_READY, &ready);

    let signal_ready = ready.clone();
    let signaler = std::thread::spawn(move || wait_and_signal(&signal_ready, signal_number));
    let spec = CommandSpec {
        argv: vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--exact".into(),
            "gate_children_reset_inherited_signal_dispositions".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ],
        timeout_seconds: 4,
        trial_timeout_seconds: None,
        approval: None,
    };
    let started = Instant::now();
    let result = orch_host::gate::run_gate(
        &format!("signal-{mode}-{signal_number}"),
        &spec,
        &root,
        &logs,
        "b306",
    );
    std::env::remove_var(HELPER_READY);
    signaler.join().unwrap().unwrap();
    let gate = result.unwrap_or_else(|error| {
        panic!(
            "gate child inherited parent disposition mode={mode} signal={signal_number}: {error:#}"
        )
    });
    assert_eq!(gate.exit_code, -1, "child should terminate by signal");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "child ignored signal {signal_number} in {mode} mode"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn gate_children_reset_inherited_signal_dispositions() {
    if std::env::var_os(HELPER_READY).is_some() {
        helper_process();
    }
    for signal_number in [SIGHUP, SIGINT, SIGTERM] {
        run_cell(SIG_DFL, "foreground", signal_number);
        run_cell(SIG_IGN, "ignored-parent", signal_number);
    }
}
