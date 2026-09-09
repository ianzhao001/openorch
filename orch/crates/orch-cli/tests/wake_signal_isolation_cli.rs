#![cfg(feature = "selfhost")]
//! B181 real-process signal matrix for both unified driver process shapes.
//! The test launches the current `orch` binary, signals only its verified
//! process group, and proves that isolated owners complete and disappear.

mod support;

use std::fs;
use std::io::Write;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

const ROUND: &str = "r181";
const WRAPPER_HARNESS: &str = "wrapper";
const DIRECT_HARNESS: &str = "direct";
const SIGINT: i32 = 2;
const SIGHUP: i32 = 1;
const PROXY_PHASE_BUDGET: Duration = Duration::from_secs(10);
const PROXY_CELL_OVERALL_BUDGET: Duration = Duration::from_secs(30);
const MANAGED_PHASE_BUDGET: Duration = Duration::from_secs(12);
const MANAGED_CELL_OVERALL_BUDGET: Duration = Duration::from_secs(36);
const MANAGED_FIXTURE_TEST: &str = "managed_provider_fixture";
const MANAGED_ENTERED_ARG: &str = "b181-managed-entered=";
const MANAGED_PID_ARG: &str = "b181-managed-pid=";
const MANAGED_PGID_ARG: &str = "b181-managed-pgid=";
const MANAGED_RELEASE_ARG: &str = "b181-managed-release=";
const MANAGED_SIGNAL_HIT_ARG: &str = "b181-managed-signal-hit=";
const MANAGED_COMPLETED_ARG: &str = "b181-managed-completed=";

fn bounded_phase_deadline(cell_overall_deadline: Instant, phase_budget: Duration) -> Instant {
    (Instant::now() + phase_budget).min(cell_overall_deadline)
}

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

unsafe extern "C" {
    fn getpgid(pid: i32) -> i32;
    fn signal(signal: i32, handler: usize) -> usize;
}

static MANAGED_FIXTURE_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_managed_fixture_signal(signal: i32) {
    MANAGED_FIXTURE_SIGNAL.store(signal, Ordering::SeqCst);
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn temp_root(label: &str) -> PathBuf {
    let orch_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("manifest path must live below the orch workspace");
    let root = orch_dir
        .join("target/test-tmp")
        .join(format!("b181-signal-{label}-{}", unique_suffix()));
    fs::create_dir_all(&root).unwrap();
    root
}

fn setup_round(root: &Path) {
    for path in ["coordination/runtime", "coordination/rounds/r181/tasks"] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    fs::copy(
        source_root.join("coordination/PROJECT-BINDING.yaml"),
        root.join("coordination/PROJECT-BINDING.yaml"),
    )
    .unwrap();
    fs::copy(source_root.join(".gitignore"), root.join(".gitignore")).unwrap();
    orch_host::ledger::append(
        root,
        ROUND,
        &[orch_host::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some(ROUND),
            serde_json::json!({
                "purpose": "wake signal isolation fixture",
                "contractSchemaVersion": 3
            }),
        )],
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r181/tasks/B181.md"),
        "---\nschemaVersion: 3\ntaskId: B181\nround: r181\nseedProtocol: verify-only\nredForm: assertion\ndependsOn: []\nentryPoints: [fixture.txt]\nseeds: []\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/rounds/**]\ngates: {fast: [testFast, testExclusive, check, checkDefault, buildDefault, buildSelfhost]}\nrequiredEvidence: [signal-isolation]\n---\nfixture\n",
    )
    .unwrap();
    fs::write(root.join("fixture.txt"), "fixture\n").unwrap();
}

fn run_fixture_git(root: &Path, args: &[&str]) {
    let output = support::fixture_git_command(root)
        .args(args)
        .output()
        .expect("start signal fixture git");
    assert!(
        output.status.success(),
        "fixture git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit_fixture_git(root: &Path, message: &str) {
    run_fixture_git(root, &["add", "-A"]);
    run_fixture_git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-qm",
            message,
        ],
    );
}

fn init_fixture_git(root: &Path) {
    run_fixture_git(root, &["init", "-q", "-b", "main"]);
    commit_fixture_git(root, "signal fixture inputs");
}

fn fixture_paths(root: &Path) -> FixturePaths {
    FixturePaths {
        entered: root.join("entered"),
        pid: root.join("owner.pid"),
        pgid: root.join("owner.pgid"),
        release: root.join("release"),
        signal_hit: root.join("signal-hit"),
        completed: root.join("completed"),
    }
}

#[derive(Clone)]
struct FixturePaths {
    entered: PathBuf,
    pid: PathBuf,
    pgid: PathBuf,
    release: PathBuf,
    signal_hit: PathBuf,
    completed: PathBuf,
}

impl FixturePaths {
    fn managed_argv(&self, program: &Path) -> Vec<String> {
        vec![
            program.to_string_lossy().into_owned(),
            "--exact".to_string(),
            MANAGED_FIXTURE_TEST.to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
            "--skip".to_string(),
            format!("{MANAGED_ENTERED_ARG}{}", self.entered.display()),
            "--skip".to_string(),
            format!("{MANAGED_PID_ARG}{}", self.pid.display()),
            "--skip".to_string(),
            format!("{MANAGED_PGID_ARG}{}", self.pgid.display()),
            "--skip".to_string(),
            format!("{MANAGED_RELEASE_ARG}{}", self.release.display()),
            "--skip".to_string(),
            format!("{MANAGED_SIGNAL_HIT_ARG}{}", self.signal_hit.display()),
            "--skip".to_string(),
            format!("{MANAGED_COMPLETED_ARG}{}", self.completed.display()),
        ]
    }
}

fn write_executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn provider_exec_script(paths: &FixturePaths) -> String {
    let argv = paths.managed_argv(&std::env::current_exe().unwrap());
    format!(
        "#!/bin/sh\nexec {provider}\n",
        provider = argv
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn write_proxy_fixture(root: &Path) -> (PathBuf, FixturePaths) {
    let paths = fixture_paths(root);
    let wrapper = root.join("coordination/scripts/wake-multica.sh");
    write_executable(&wrapper, &provider_exec_script(&paths));
    let configured = root.join("bin/smartclaw");
    write_executable(&configured, "#!/bin/sh\nexit 99\n");
    (configured, paths)
}

fn write_managed_fixture(root: &Path) -> (PathBuf, FixturePaths) {
    let paths = fixture_paths(root);
    let executable = root.join("bin/opencode");
    // The shim immediately execs the compiled helper. The stable provider is
    // therefore one PID/PGID with no polling shell children, while the config
    // still names the absolute executable selected by the OpenCode driver.
    write_executable(&executable, &provider_exec_script(&paths));
    (executable, paths)
}

fn write_harness_config(root: &Path, alias: &str, driver: &str, executable: &Path) {
    let local = root.join(".orch");
    fs::create_dir_all(&local).unwrap();
    let defaults = if driver == "opencode" {
        "    defaults: {provider: local, model: model-a, effort: high}\n"
    } else {
        ""
    };
    let config = format!(
        "version: 1\nharnesses:\n  {alias}:\n    driver: {driver}\n    executable: {}\n    enabled: true\n{defaults}    cwdPolicy: project-root\n",
        executable.display()
    );
    assert!(!config.contains("argv:"));
    assert!(!config.contains("env:"));
    assert!(!config.contains("PATH"));
    assert!(!config.contains("capabilit"));
    fs::write(local.join("harnesses.yaml"), config).unwrap();
    let ignored = support::fixture_git_command(root)
        .args(["check-ignore", "-q", ".orch/harnesses.yaml"])
        .status()
        .unwrap();
    assert!(
        ignored.success(),
        "fixture harness config must be gitignored"
    );
}

fn managed_fixture_path(arguments: &[String], prefix: &str) -> PathBuf {
    let matches = arguments
        .iter()
        .filter_map(|argument| argument.strip_prefix(prefix).map(PathBuf::from))
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "managed fixture invocation must carry exactly one {prefix} argument"
    );
    matches.into_iter().next().unwrap()
}

fn check_managed_fixture_signal(signal_hit: &Path) {
    let signal = MANAGED_FIXTURE_SIGNAL.load(Ordering::SeqCst);
    if signal != 0 {
        fs::write(signal_hit, format!("signal={signal}\n")).unwrap();
        std::process::exit(97);
    }
}

#[test]
fn managed_provider_fixture() {
    let arguments = std::env::args().collect::<Vec<_>>();
    if !arguments
        .iter()
        .any(|argument| argument.starts_with("b181-managed-"))
    {
        // This helper is part of the default integration-test target so the
        // copied binary can address it by exact name. Without fixture markers
        // it is deliberately a no-op, not another signal-matrix cell.
        return;
    }
    let entered = managed_fixture_path(&arguments, MANAGED_ENTERED_ARG);
    let pid_path = managed_fixture_path(&arguments, MANAGED_PID_ARG);
    let pgid_path = managed_fixture_path(&arguments, MANAGED_PGID_ARG);
    let release = managed_fixture_path(&arguments, MANAGED_RELEASE_ARG);
    let signal_hit = managed_fixture_path(&arguments, MANAGED_SIGNAL_HIT_ARG);
    let completed = managed_fixture_path(&arguments, MANAGED_COMPLETED_ARG);

    let pid = std::process::id();
    let pgid = process_group(pid).expect("managed fixture must have a process group");
    assert_eq!(
        pid, pgid,
        "managed fixture must lead its exact process group"
    );
    fs::write(&pid_path, format!("{pid}\n")).unwrap();
    fs::write(&pgid_path, format!("{pgid}\n")).unwrap();
    orch_host::gate::register_gate_fixture(
        std::env::var_os(orch_host::gate::ORCH_GATE_FIXTURE_REGISTRY).as_deref(),
        pid,
        pgid,
    )
    .expect("register managed fixture in inherited gate registry");
    MANAGED_FIXTURE_SIGNAL.store(0, Ordering::SeqCst);
    // SAFETY: both fixed POSIX signals call a handler that only stores an i32
    // in a lock-free atomic. File IO and process exit happen back in normal
    // Rust control flow after observing that atomic.
    for signal_number in [SIGINT, SIGHUP] {
        let previous = unsafe {
            signal(
                signal_number,
                record_managed_fixture_signal as *const () as usize,
            )
        };
        assert_ne!(
            previous,
            usize::MAX,
            "install managed fixture signal handler for {signal_number}"
        );
    }
    fs::write(&entered, b"entered\n").unwrap();

    let release_deadline = Instant::now() + MANAGED_CELL_OVERALL_BUDGET;
    while !release.is_file() {
        check_managed_fixture_signal(&signal_hit);
        assert!(
            Instant::now() < release_deadline,
            "managed fixture release timed out: {}",
            release.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    check_managed_fixture_signal(&signal_hit);

    println!(
        "\n{}",
        r#"{"type":"step_start","sessionID":"ses_b181","part":{"type":"step-start"}}"#
    );
    println!(
        "{}",
        r#"{"type":"step_finish","sessionID":"ses_b181","part":{"type":"step-finish","reason":"stop"}}"#
    );
    std::io::stdout().flush().unwrap();
    fs::write(completed, b"completed\n").unwrap();
}

#[test]
fn managed_fixture_uses_one_compiled_provider_without_shell_children() {
    let root = temp_root("managed-shape");
    let (program, paths) = write_managed_fixture(&root);
    let shim = fs::read_to_string(&program).unwrap();
    assert!(shim.starts_with("#!/bin/sh\nexec "));
    assert!(shim.contains(MANAGED_FIXTURE_TEST));
    assert!(!shim.contains("/bin/sleep"));
    assert!(!shim.contains("while "));
    let argv = paths.managed_argv(&std::env::current_exe().unwrap());
    assert_eq!(argv[1], "--exact");
    assert_eq!(argv[2], MANAGED_FIXTURE_TEST);
    assert!(!argv.iter().any(|argument| argument == "/bin/sh"));
    fs::remove_dir_all(root).unwrap();
}

fn saturated_stdout() -> (UnixStream, Stdio) {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    writer.set_nonblocking(true).unwrap();
    let bytes = [b'x'; 8192];
    loop {
        match writer.write(&bytes) {
            Ok(0) => panic!("stdout saturation wrote zero bytes"),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("stdout saturation failed: {error}"),
        }
    }
    writer.set_nonblocking(false).unwrap();
    let raw = writer.into_raw_fd();
    // SAFETY: ownership of the live writer descriptor moves exactly once from
    // UnixStream into File, which Stdio then owns until Command::spawn.
    let file = unsafe { fs::File::from_raw_fd(raw) };
    (reader, Stdio::from(file))
}

fn start_outer(root: &Path, harness: &str) -> (Child, UnixStream) {
    let (stdout_blocker, stdout) = saturated_stdout();
    let mut command = fixture_orch_command(&[]);
    command
        .arg("--allow-stale-binary")
        .arg("--root")
        .arg(root)
        .args(["wake", harness, "--message", "B181 signal fixture"])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .process_group(0);
    (
        command.spawn().expect("spawn real orch wake"),
        stdout_blocker,
    )
}

fn command_success(program: &str, args: &[String]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn process_group(pid: u32) -> Option<u32> {
    let pid = i32::try_from(pid).ok()?;
    // SAFETY: getpgid reads kernel process metadata and does not mutate memory.
    let pgid = unsafe { getpgid(pid) };
    (pgid > 0).then_some(pgid as u32)
}

fn pid_alive(pid: u32) -> bool {
    command_success("/bin/kill", &["-0".into(), pid.to_string()])
}

fn group_alive(pgid: u32) -> bool {
    command_success("/bin/kill", &["-0".into(), "--".into(), format!("-{pgid}")])
}

fn signal_group(pgid: u32, signal: i32) {
    let flag = match signal {
        SIGINT => "-INT",
        SIGHUP => "-HUP",
        15 => "-TERM",
        9 => "-KILL",
        _ => panic!("unsupported test signal {signal}"),
    };
    signal_group_flag(pgid, flag);
}

fn signal_group_flag(pgid: u32, flag: &str) {
    let status = Command::new("/bin/kill")
        .args([flag, "--", &format!("-{pgid}")])
        .status()
        .expect("run /bin/kill");
    assert!(status.success(), "{flag} failed for exact pgid {pgid}");
}

fn wait_file(path: &Path, deadline: Instant, label: &str) {
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{label} timed out: {}", path.display());
}

fn read_u32(path: &Path) -> u32 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

fn wait_child_exit(child: &mut Child, deadline: Instant, label: &str) -> ExitStatus {
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{label} did not exit before deadline");
}

fn wait_pid_and_group_gone(pid: u32, pgid: u32, deadline: Instant, label: &str) {
    while Instant::now() < deadline {
        if !pid_alive(pid) && !group_alive(pgid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "{label} remained after deadline: pid={pid} pidAlive={} pgid={pgid} groupAlive={}",
        pid_alive(pid),
        group_alive(pgid)
    );
}

fn find_waiting_reap(root: &Path, owner_kind: &str, deadline: Instant) -> (PathBuf, u32, u32) {
    let dir = root.join("coordination/runtime/supervisors");
    while Instant::now() < deadline {
        if let Ok(entries) = fs::read_dir(&dir) {
            for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
                if !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".reap.json"))
                {
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue;
                };
                if value["state"] == "waiting" && value["ownerKind"] == owner_kind {
                    return (
                        path,
                        value["pid"].as_u64().unwrap() as u32,
                        value["pgid"].as_u64().unwrap() as u32,
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("waiting {owner_kind} reap sidecar timed out");
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn ledger_events(root: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(root.join(format!("coordination/rounds/{ROUND}/events.jsonl")))
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_no_legacy_registry(root: &Path) {
    assert!(!root.join("coordination/agents.yaml").exists());
    assert!(!root.join("coordination/harnesses.yaml").exists());
    assert!(!root.join("coordination/adapters").exists());
}

fn supervisor_status_path(reap_path: &Path) -> PathBuf {
    let reap_name = reap_path.file_name().unwrap().to_string_lossy();
    let token = reap_name.strip_suffix(".reap.json").unwrap();
    reap_path.with_file_name(format!("{token}.status.json"))
}

fn assert_wake_owner_binding(
    root: &Path,
    wake: &serde_json::Value,
    reap_path: &Path,
    supervisor_pid: u32,
    provider_pid: u32,
) {
    let wake_id = wake["wakeId"].as_str().unwrap();
    assert_eq!(wake["controlWakeId"], wake_id);
    assert_eq!(wake["pid"], provider_pid);
    let control = read_json(&root.join(format!(
        "coordination/runtime/supervisors/{wake_id}.control.json"
    )));
    let token = reap_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_suffix(".reap.json")
        .unwrap()
        .to_string();
    assert_eq!(control["wakeId"], wake_id);
    assert_eq!(control["token"], token);
    assert_eq!(control["supervisorPid"], supervisor_pid);
    assert_eq!(control["providerPid"], provider_pid);
    assert_eq!(
        control["statusPath"],
        supervisor_status_path(reap_path).display().to_string()
    );
}

fn atomic_artifacts_absent(root: &Path) -> bool {
    let ledger = root.join(format!("coordination/rounds/{ROUND}/events.jsonl"));
    let wal = root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl"));
    let intent = root.join(format!(
        "coordination/runtime/ledger-wal/.{ROUND}.atomic-batch-intent.json"
    ));
    !intent.exists()
        && [ledger.parent().unwrap(), wal.parent().unwrap()]
            .into_iter()
            .all(|directory| {
                !fs::read_dir(directory)
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.contains(".atomic-batch-tmp-"))
                    })
            })
}

fn wait_consistent_ledger_storage(root: &Path, deadline: Instant) -> Vec<u8> {
    let ledger = root.join(format!("coordination/rounds/{ROUND}/events.jsonl"));
    let wal = root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl"));
    while Instant::now() < deadline {
        if let (Ok(ledger_bytes), Ok(wal_bytes)) = (fs::read(&ledger), fs::read(&wal)) {
            if ledger_bytes == wal_bytes && atomic_artifacts_absent(root) {
                return ledger_bytes;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("ledger/WAL did not reach a clean identical snapshot");
}

fn wait_wake_payload(root: &Path, alias: &str, deadline: Instant) -> serde_json::Value {
    while Instant::now() < deadline {
        let wakes = ledger_events(root)
            .into_iter()
            .filter(|event| event["type"] == "WakeIssued" && event["payload"]["harness"] == alias)
            .collect::<Vec<_>>();
        match wakes.as_slice() {
            [wake] => return wake["payload"].clone(),
            [] => std::thread::sleep(Duration::from_millis(5)),
            _ => panic!("duplicate {alias} WakeIssued: {}", wakes.len()),
        }
    }
    panic!("{alias} WakeIssued was not published before deadline");
}

struct ExactCleanup {
    release: PathBuf,
    groups: Vec<u32>,
}

impl ExactCleanup {
    fn new(paths: &FixturePaths, outer_pgid: u32) -> Self {
        Self {
            release: paths.release.clone(),
            groups: vec![outer_pgid],
        }
    }

    fn track(&mut self, pgid: u32) {
        if !self.groups.contains(&pgid) {
            self.groups.push(pgid);
        }
    }

    fn disarm(&mut self) {
        self.groups.clear();
    }
}

impl Drop for ExactCleanup {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, b"cleanup\n");
        let grace = Instant::now() + Duration::from_millis(300);
        while Instant::now() < grace && self.groups.iter().any(|pgid| group_alive(*pgid)) {
            std::thread::sleep(Duration::from_millis(5));
        }
        for pgid in self
            .groups
            .iter()
            .copied()
            .filter(|pgid| group_alive(*pgid))
        {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", "--", &format!("-{pgid}")])
                .status();
        }
        std::thread::sleep(Duration::from_millis(50));
        for pgid in self
            .groups
            .iter()
            .copied()
            .filter(|pgid| group_alive(*pgid))
        {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .status();
        }
    }
}

fn setup_channel_fixture(root: &Path, wrapper: bool) -> (&'static str, FixturePaths) {
    setup_round(root);
    let (alias, driver, executable, paths) = if wrapper {
        let (executable, paths) = write_proxy_fixture(root);
        (WRAPPER_HARNESS, "smartclaw", executable, paths)
    } else {
        let (executable, paths) = write_managed_fixture(root);
        (DIRECT_HARNESS, "opencode", executable, paths)
    };
    init_fixture_git(root);
    orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("B181 signal fixture")).unwrap();
    commit_fixture_git(root, "signal fixture signed state");
    write_harness_config(root, alias, driver, &executable);
    assert_no_legacy_registry(root);
    (alias, paths)
}

fn assert_distinct_channel_groups(
    outer_pgid: u32,
    provider_pid: u32,
    provider_pgid: u32,
    supervisor_pid: u32,
    supervisor_pgid: u32,
) {
    assert_eq!(provider_pid, provider_pgid);
    assert_eq!(supervisor_pid, supervisor_pgid);
    assert_ne!(outer_pgid, supervisor_pgid);
    assert_ne!(outer_pgid, provider_pgid);
    assert_ne!(supervisor_pgid, provider_pgid);
}

fn wait_bound_wake_storage(
    root: &Path,
    alias: &str,
    wrapper: bool,
    reap_path: &Path,
    supervisor_pid: u32,
    provider_pid: u32,
    deadline: Instant,
) -> Vec<u8> {
    let wake = wait_wake_payload(root, alias, deadline);
    assert_eq!(wake["method"], "unified-channel-v1");
    assert_eq!(wake["action"], "consult");
    assert_eq!(
        wake["driver"],
        if wrapper { "smartclaw" } else { "opencode" }
    );
    assert_eq!(wake["cwdSelection"], "project-root");
    assert_wake_owner_binding(root, &wake, reap_path, supervisor_pid, provider_pid);
    wait_consistent_ledger_storage(root, deadline)
}

fn run_proxy_cell(iteration: usize, signal: i32) {
    let cell_overall_deadline = Instant::now() + PROXY_CELL_OVERALL_BUDGET;
    let root = temp_root(&format!("wrapper-{iteration}-{signal}"));
    let (alias, paths) = setup_channel_fixture(&root, true);
    let (mut outer, stdout_blocker) = start_outer(&root, alias);
    let outer_pgid = outer.id();
    let mut cleanup = ExactCleanup::new(&paths, outer_pgid);
    assert_eq!(process_group(outer.id()), Some(outer.id()));

    let entered_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    wait_file(&paths.entered, entered_deadline, "wrapper provider entered");
    let provider_pid = read_u32(&paths.pid);
    let provider_pgid = read_u32(&paths.pgid);
    cleanup.track(provider_pgid);
    let receipt_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    let (reap_path, supervisor_pid, supervisor_pgid) =
        find_waiting_reap(&root, "managedSupervisor", receipt_deadline);
    cleanup.track(supervisor_pgid);
    assert_distinct_channel_groups(
        outer_pgid,
        provider_pid,
        provider_pgid,
        supervisor_pid,
        supervisor_pgid,
    );
    let storage_before = wait_bound_wake_storage(
        &root,
        alias,
        true,
        &reap_path,
        supervisor_pid,
        provider_pid,
        receipt_deadline,
    );
    assert!(outer.try_wait().unwrap().is_none());

    signal_group(outer_pgid, signal);
    let outer_exit_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    let status = wait_child_exit(&mut outer, outer_exit_deadline, "outer wrapper orch");
    assert_eq!(status.signal(), Some(signal));
    wait_pid_and_group_gone(
        outer.id(),
        outer_pgid,
        outer_exit_deadline,
        "outer wrapper orch",
    );
    drop(stdout_blocker);
    assert!(pid_alive(supervisor_pid));
    assert!(group_alive(supervisor_pgid));
    assert!(pid_alive(provider_pid));
    assert!(group_alive(provider_pgid));
    assert!(!paths.signal_hit.exists());
    assert_eq!(
        wait_consistent_ledger_storage(&root, outer_exit_deadline),
        storage_before
    );

    fs::write(&paths.release, b"release\n").unwrap();
    let completion_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    wait_file(
        &paths.completed,
        completion_deadline,
        "wrapper provider completed",
    );
    let status_path = supervisor_status_path(&reap_path);
    wait_file(
        &status_path,
        completion_deadline,
        "wrapper supervisor status",
    );
    let supervisor_status = read_json(&status_path);
    assert_eq!(supervisor_status["managedScopeTerminated"], true);
    assert!(supervisor_status["error"].is_null());
    let group_gone_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    wait_pid_and_group_gone(
        provider_pid,
        provider_pgid,
        group_gone_deadline,
        "wrapper provider",
    );
    wait_pid_and_group_gone(
        supervisor_pid,
        supervisor_pgid,
        group_gone_deadline,
        "wrapper supervisor",
    );
    assert!(!paths.signal_hit.exists());
    assert_eq!(read_json(&reap_path)["state"], "waiting");
    assert_no_legacy_registry(&root);
    assert_eq!(
        ledger_events(&root)
            .iter()
            .filter(|event| event["type"] == "WakeIssued")
            .count(),
        1
    );
    cleanup.disarm();
    fs::remove_dir_all(root).unwrap();
}

fn run_managed_cell(iteration: usize, signal: i32) {
    let cell_overall_deadline = Instant::now() + MANAGED_CELL_OVERALL_BUDGET;
    let root = temp_root(&format!("direct-{iteration}-{signal}"));
    let (alias, paths) = setup_channel_fixture(&root, false);
    let (mut outer, stdout_blocker) = start_outer(&root, alias);
    let outer_pgid = outer.id();
    let mut cleanup = ExactCleanup::new(&paths, outer_pgid);
    assert_eq!(process_group(outer.id()), Some(outer.id()));

    let entered_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    wait_file(&paths.entered, entered_deadline, "managed provider entered");
    let provider_pid = read_u32(&paths.pid);
    let provider_pgid = read_u32(&paths.pgid);
    cleanup.track(provider_pgid);
    let receipt_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    let (reap_path, supervisor_pid, supervisor_pgid) =
        find_waiting_reap(&root, "managedSupervisor", receipt_deadline);
    cleanup.track(supervisor_pgid);
    assert_distinct_channel_groups(
        outer_pgid,
        provider_pid,
        provider_pgid,
        supervisor_pid,
        supervisor_pgid,
    );
    let storage_before = wait_bound_wake_storage(
        &root,
        alias,
        false,
        &reap_path,
        supervisor_pid,
        provider_pid,
        receipt_deadline,
    );
    assert!(outer.try_wait().unwrap().is_none());

    signal_group(outer_pgid, signal);
    let outer_exit_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    let status = wait_child_exit(&mut outer, outer_exit_deadline, "outer managed orch");
    assert_eq!(status.signal(), Some(signal));
    wait_pid_and_group_gone(
        outer.id(),
        outer_pgid,
        outer_exit_deadline,
        "outer managed orch",
    );
    drop(stdout_blocker);
    assert!(pid_alive(supervisor_pid));
    assert!(group_alive(supervisor_pgid));
    assert!(pid_alive(provider_pid));
    assert!(group_alive(provider_pgid));
    assert!(!paths.signal_hit.exists());
    assert_eq!(
        wait_consistent_ledger_storage(&root, outer_exit_deadline),
        storage_before
    );

    fs::write(&paths.release, b"release\n").unwrap();
    let completion_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    wait_file(
        &paths.completed,
        completion_deadline,
        "managed provider completed",
    );
    let status_path = supervisor_status_path(&reap_path);
    wait_file(
        &status_path,
        completion_deadline,
        "managed supervisor status",
    );
    let supervisor_status = read_json(&status_path);
    assert_eq!(supervisor_status["managedScopeTerminated"], true);
    assert!(supervisor_status["error"].is_null());
    let group_gone_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    wait_pid_and_group_gone(
        provider_pid,
        provider_pgid,
        group_gone_deadline,
        "managed provider",
    );
    wait_pid_and_group_gone(
        supervisor_pid,
        supervisor_pgid,
        group_gone_deadline,
        "hidden supervisor",
    );
    assert!(!paths.signal_hit.exists());
    assert_eq!(read_json(&reap_path)["state"], "waiting");
    assert_no_legacy_registry(&root);
    assert_eq!(
        ledger_events(&root)
            .iter()
            .filter(|event| event["type"] == "WakeIssued")
            .count(),
        1
    );
    cleanup.disarm();
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "testExclusive:four_signal_topology_cells_pass_ten_consecutive_runs"]
fn four_signal_topology_cells_pass_ten_consecutive_runs() {
    for iteration in 0..10 {
        for signal in [SIGINT, SIGHUP] {
            run_proxy_cell(iteration, signal);
            run_managed_cell(iteration, signal);
        }
    }
}
