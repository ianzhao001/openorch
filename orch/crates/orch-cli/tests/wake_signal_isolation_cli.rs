//! B181 real-process signal matrix for both durable wake-owner topologies.
//! The test launches the current `orch` binary, signals only its verified
//! process group, and proves that isolated owners complete and disappear.

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

const ROUND: &str = "r181";
const PROXY_AGENT: &str = "executor-claw";
const MANAGED_AGENT: &str = "executor-opencode";
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
    for path in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r181/tasks",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 2, quota: 2, roles: [implement, primary-review]}
    executor-opencode: {agent: 2, quota: 2, roles: [implement, secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r181/tasks/B181.md"),
        "---\ntaskId: B181\nround: r181\nagent: executor-opencode\nseedProtocol: pure-spec\nentryPoints: [fixture.txt]\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {fast: [testFast]}\nbudgets: {wallMinutes: 10}\nrequiredReviews:\n  - {role: primary, agent: executor-claw}\nrequiredEvidence: [signal-isolation]\n---\nfixture\n",
    )
    .unwrap();
    orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("B181 signal fixture")).unwrap();
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
    fn argv(&self, program: &Path) -> Vec<String> {
        [
            program.to_path_buf(),
            self.entered.clone(),
            self.pid.clone(),
            self.pgid.clone(),
            self.release.clone(),
            self.signal_hit.clone(),
            self.completed.clone(),
        ]
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
    }

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
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_proxy_fixture(root: &Path) -> (PathBuf, FixturePaths) {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("wake-multica.sh");
    write_executable(
        &script,
        r#"#!/bin/sh
entered=$1
pid_file=$2
pgid_file=$3
release=$4
signal_hit=$5
completed=$6
trap 'printf "signal\n" > "$signal_hit"; exit 97' INT HUP
printf '%s\n' "$$" > "$pid_file"
printf '%s\n' "$$" > "$pgid_file"
: > "$entered"
while [ ! -f "$release" ]; do /bin/sleep 0.02; done
printf '{"type":"text","sessionId":"%s","text":"received"}\n' "$ORCH_MULTICA_SESSION"
: > "$completed"
"#,
    );
    (script, fixture_paths(root))
}

fn write_managed_fixture(root: &Path) -> (PathBuf, FixturePaths) {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("opencode");
    // The production OFFER/ACCEPT sandwich intentionally rejects process-tree
    // drift. A shell polling a release file with short-lived sleep children
    // changes membership inside that sandwich, so the fixture itself can violate
    // the identity contract it is meant to exercise. Reuse this compiled test
    // binary as one stable provider process; the helper below emits the same
    // OpenCode receipt after release without weakening production validation.
    fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    (executable, fixture_paths(root))
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
    let source = include_str!("wake_signal_isolation_cli.rs");
    let start = source.find("fn write_managed_fixture(").unwrap();
    let end = source[start..]
        .find("\n#[test]\nfn managed_fixture_uses_one_compiled_provider_without_shell_children")
        .unwrap()
        + start;
    let fixture = &source[start..end];
    assert!(fixture.contains("fs::copy(std::env::current_exe()"));
    assert!(fixture.contains("std::thread::sleep(Duration::from_millis(20))"));
    assert!(!fixture.contains("#!/bin/sh"));
    assert!(!fixture.contains("/bin/sleep"));
    assert!(!fixture.contains("Command::new"));

    let root = temp_root("managed-shape");
    let (program, paths) = write_managed_fixture(&root);
    let argv = paths.managed_argv(&program);
    assert_eq!(Path::new(&argv[0]).file_name().unwrap(), "opencode");
    assert_eq!(argv[1], "--exact");
    assert_eq!(argv[2], MANAGED_FIXTURE_TEST);
    assert_eq!(argv[3], "--nocapture");
    assert_eq!(argv[4], "--test-threads=1");
    assert!(!argv.iter().any(|argument| argument == "/bin/sh"));
    fs::remove_dir_all(root).unwrap();
}

fn write_agent(root: &Path, agent: &str, argv: &[String]) {
    let value = serde_json::json!({
        "agents": {
            agent: {
                "injectable": true,
                "sessionId": "b181-session",
                "wake": {"argv": argv},
                "pokeHint": ""
            }
        }
    });
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .unwrap();
}

fn start_outer(root: &Path, agent: &str) -> Child {
    let mut command = fixture_orch_command(&[]);
    command
        .arg("--root")
        .arg(root)
        .args(["wake", agent, "--message", "B181 signal fixture"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let child = command.spawn().expect("spawn real orch wake");
    assert_eq!(process_group(child.id()), Some(child.id()));
    child
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

struct ExactCleanup {
    release: PathBuf,
    groups: Vec<u32>,
}

impl ExactCleanup {
    fn new(release: PathBuf, outer_pgid: u32) -> Self {
        Self {
            release,
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

fn run_proxy_cell(iteration: usize, signal: i32) {
    let cell_overall_deadline = Instant::now() + PROXY_CELL_OVERALL_BUDGET;
    let root = temp_root(&format!("proxy-{iteration}-{signal}"));
    setup_round(&root);
    let (program, paths) = write_proxy_fixture(&root);
    let mut argv = vec!["/bin/sh".to_string()];
    argv.extend(paths.argv(&program));
    write_agent(&root, PROXY_AGENT, &argv);

    let mut outer = start_outer(&root, PROXY_AGENT);
    let outer_pgid = outer.id();
    let mut cleanup = ExactCleanup::new(paths.release.clone(), outer_pgid);
    let entered_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    wait_file(&paths.entered, entered_deadline, "proxy entered");
    let wrapper_pid = read_u32(&paths.pid);
    let wrapper_pgid = read_u32(&paths.pgid);
    cleanup.track(wrapper_pgid);
    let receipt_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    let (reap_path, reaped_pid, reaped_pgid) =
        find_waiting_reap(&root, "wakeLogProxy", receipt_deadline);
    assert_eq!((reaped_pid, reaped_pgid), (wrapper_pid, wrapper_pgid));
    assert_eq!(wrapper_pid, wrapper_pgid);
    assert_ne!(outer_pgid, wrapper_pgid);

    signal_group(outer_pgid, signal);
    let outer_exit_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    let status = wait_child_exit(&mut outer, outer_exit_deadline, "outer proxy orch");
    assert_eq!(status.signal(), Some(signal));
    assert!(pid_alive(wrapper_pid));
    assert!(group_alive(wrapper_pgid));
    assert!(!paths.signal_hit.exists());

    fs::write(&paths.release, b"release\n").unwrap();
    let completion_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    wait_file(&paths.completed, completion_deadline, "proxy completed");
    let group_gone_deadline = bounded_phase_deadline(cell_overall_deadline, PROXY_PHASE_BUDGET);
    wait_pid_and_group_gone(
        wrapper_pid,
        wrapper_pgid,
        group_gone_deadline,
        "proxy wrapper",
    );
    assert!(!paths.signal_hit.exists());
    assert_eq!(read_json(&reap_path)["state"], "waiting");
    cleanup.disarm();
    fs::remove_dir_all(root).unwrap();
}

fn run_managed_cell(iteration: usize, signal: i32) {
    let cell_overall_deadline = Instant::now() + MANAGED_CELL_OVERALL_BUDGET;
    let root = temp_root(&format!("managed-{iteration}-{signal}"));
    setup_round(&root);
    let (program, paths) = write_managed_fixture(&root);
    write_agent(&root, MANAGED_AGENT, &paths.managed_argv(&program));

    let mut outer = start_outer(&root, MANAGED_AGENT);
    let outer_pgid = outer.id();
    let mut cleanup = ExactCleanup::new(paths.release.clone(), outer_pgid);
    let entered_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    wait_file(&paths.entered, entered_deadline, "managed provider entered");
    let provider_pid = read_u32(&paths.pid);
    let provider_pgid = read_u32(&paths.pgid);
    cleanup.track(provider_pgid);
    let receipt_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    let (reap_path, supervisor_pid, supervisor_pgid) =
        find_waiting_reap(&root, "managedSupervisor", receipt_deadline);
    cleanup.track(supervisor_pgid);
    assert_eq!(provider_pid, provider_pgid);
    assert_eq!(supervisor_pid, supervisor_pgid);
    assert_ne!(outer_pgid, supervisor_pgid);
    assert_ne!(outer_pgid, provider_pgid);
    assert_ne!(supervisor_pgid, provider_pgid);

    signal_group(outer_pgid, signal);
    let outer_exit_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    let status = wait_child_exit(&mut outer, outer_exit_deadline, "outer managed orch");
    assert_eq!(status.signal(), Some(signal));
    assert!(pid_alive(supervisor_pid));
    assert!(pid_alive(provider_pid));
    assert!(!paths.signal_hit.exists());

    fs::write(&paths.release, b"release\n").unwrap();
    let completion_deadline = bounded_phase_deadline(cell_overall_deadline, MANAGED_PHASE_BUDGET);
    wait_file(
        &paths.completed,
        completion_deadline,
        "managed provider completed",
    );
    let reap_name = reap_path.file_name().unwrap().to_string_lossy();
    let token = reap_name.strip_suffix(".reap.json").unwrap();
    let status_path = reap_path.with_file_name(format!("{token}.status.json"));
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
