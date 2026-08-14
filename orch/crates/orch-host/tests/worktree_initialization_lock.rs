//! B249 contract seed: every orch-host worktree initializer sharing one physical
//! Git common-dir must share one blocking same-process and cross-process lock.
//!
//! Expected red: compile (`E0432`) because the shared doc-hidden
//! `orch_host::gate::b249_contract` namespace is absent.  Both B249 seed targets use
//! that same missing namespace so Cargo parallel target ordering cannot change the
//! canonical compile identity.  The namespace only re-exports real production
//! functions and observation seams; it is not a test-only lock API.
//!
//! Negative mutations required by the task card:
//! - M5: key by caller root instead of canonical `--git-common-dir`;
//! - M6: lock only named or only detached worktree creation;
//! - M7: omit either the keyed in-process Mutex or file lock, use `try_write`,
//!   or release before Git exits;
//! - M8: replace the per-common-dir lock with one global lock.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use orch_host::gate::b249_contract::{
    canonical_worktree_common_dir, worktree_add, worktree_add_detached,
};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        Self(orch_host::util::test_scratch_dir(&format!("b249-{label}")))
    }

    fn join(&self, child: &str) -> std::path::PathBuf {
        self.0.join(child)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn install_fake_git(root: &TempDir) -> std::path::PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create fake bin");
    let git = bin.join("git");
    let script = r#"#!/bin/sh
caller=
if [ "$1" = "-C" ]; then
  caller=$2
  shift 2
fi
common=$ORCH_FAKE_COMMON_DIR
active=$ORCH_FAKE_ACTIVE
if [ -n "$ORCH_FAKE_CALLER_A" ] && [ "$caller" = "$ORCH_FAKE_CALLER_A" ]; then
  common=$ORCH_FAKE_COMMON_DIR_A
  active=$ORCH_FAKE_ACTIVE_A
elif [ -n "$ORCH_FAKE_CALLER_B" ] && [ "$caller" = "$ORCH_FAKE_CALLER_B" ]; then
  common=$ORCH_FAKE_COMMON_DIR_B
  active=$ORCH_FAKE_ACTIVE_B
fi
if [ "$1" = "rev-parse" ] && [ "$2" = "--git-common-dir" ]; then
  printf '%s\n' "$common"
  exit 0
fi
if [ "$1" = "worktree" ] && [ "$2" = "add" ]; then
  if /bin/mkdir "$active" 2>/dev/null; then
    printf 'entered:%s\n' "$ORCH_FAKE_WORKER" >> "$ORCH_FAKE_LOG"
    while [ ! -e "$ORCH_FAKE_RELEASE" ]; do /bin/sleep 0.01; done
    /bin/rmdir "$active" 2>/dev/null || true
    exit 0
  fi
  printf 'concurrent git worktree add escaped lock\n' >&2
  exit 73
fi
printf 'unexpected fake git argv: %s\n' "$*" >&2
exit 99
"#;
    fs::write(&git, script).expect("write fake git");
    let mut permissions = fs::metadata(&git).expect("fake git metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&git, permissions).expect("chmod fake git");
    bin
}

fn worker_environment_present() -> bool {
    std::env::var_os("ORCH_B249_LOCK_WORKER").is_some()
}

#[test]
fn worktree_lock_worker_named() {
    if !worker_environment_present() {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_CALLER_ROOT").unwrap());
    let common = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_COMMON_DIR").unwrap());
    assert_eq!(canonical_worktree_common_dir(&root).unwrap(), common);
    fs::write(std::env::var_os("ORCH_FAKE_READY").unwrap(), b"ready\n").unwrap();
    worktree_add(&root, &root.join("named-site"), "task/fake", "deadbeef")
        .expect("named worktree add");
}

#[test]
fn worktree_lock_worker_detached() {
    if !worker_environment_present() {
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_CALLER_ROOT").unwrap());
    let common = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_COMMON_DIR").unwrap());
    assert_eq!(canonical_worktree_common_dir(&root).unwrap(), common);
    fs::write(std::env::var_os("ORCH_FAKE_READY").unwrap(), b"ready\n").unwrap();
    worktree_add_detached(&root, &root.join("detached-site"), "deadbeef")
        .expect("detached worktree add");
}

struct WorkerSpec<'a> {
    test_name: &'a str,
    caller: &'a std::path::Path,
    common: &'a std::path::Path,
    active: &'a std::path::Path,
    log: &'a std::path::Path,
    release: &'a std::path::Path,
    ready: &'a std::path::Path,
    bin: &'a std::path::Path,
    label: &'a str,
}

fn spawn_worker(spec: WorkerSpec<'_>) -> Child {
    Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", spec.test_name, "--nocapture"])
        .env("ORCH_B249_LOCK_WORKER", "1")
        .env("ORCH_FAKE_CALLER_ROOT", spec.caller)
        .env("ORCH_FAKE_COMMON_DIR", spec.common)
        .env("ORCH_FAKE_ACTIVE", spec.active)
        .env("ORCH_FAKE_LOG", spec.log)
        .env("ORCH_FAKE_RELEASE", spec.release)
        .env("ORCH_FAKE_READY", spec.ready)
        .env("ORCH_FAKE_WORKER", spec.label)
        .env("PATH", spec.bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker test")
}

fn wait_for_lines(path: &std::path::Path, wanted: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let count = fs::read_to_string(path).unwrap_or_default().lines().count();
        if count >= wanted {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_path(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

fn poll_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll worker") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_success(status: ExitStatus, name: &str) {
    assert!(status.success(), "{name} worker failed with {status}");
}

#[test]
fn worktree_lock_thread_worker() {
    let Some(mode) = std::env::var_os("ORCH_B249_THREAD_MODE") else {
        return;
    };
    let mode = mode.to_string_lossy().into_owned();
    let caller_a = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_CALLER_A").unwrap());
    let caller_b = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_CALLER_B").unwrap());
    let common_a = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_COMMON_DIR_A").unwrap());
    let common_b = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_COMMON_DIR_B").unwrap());
    let log = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_LOG").unwrap());
    let release = std::path::PathBuf::from(std::env::var_os("ORCH_FAKE_RELEASE").unwrap());

    assert_eq!(canonical_worktree_common_dir(&caller_a).unwrap(), common_a);
    assert_eq!(canonical_worktree_common_dir(&caller_b).unwrap(), common_b);

    let (ready_tx, ready_rx) = mpsc::channel();
    let first_caller = caller_a.clone();
    let first = thread::spawn(move || {
        ready_tx.send("named").unwrap();
        worktree_add(
            &first_caller,
            &first_caller.join("thread-named-site"),
            "task/thread-fake",
            "deadbeef",
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("named thread readiness");
    if !wait_for_lines(&log, 1, Duration::from_secs(5)) {
        fs::write(&release, b"release\n").unwrap();
        let first_result = first.join().expect("join named thread");
        panic!("named thread never reached fake git: {first_result:?}");
    }

    let (ready_tx, ready_rx) = mpsc::channel();
    let second_caller = caller_b.clone();
    let second = thread::spawn(move || {
        ready_tx.send("detached").unwrap();
        worktree_add_detached(
            &second_caller,
            &second_caller.join("thread-detached-site"),
            "deadbeef",
        )
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("detached thread readiness");

    let same_common = mode == "same-common";
    let second_escaped = if same_common {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !second.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        second.is_finished()
    } else {
        false
    };
    let both_entered_before_release = if same_common {
        false
    } else {
        wait_for_lines(&log, 2, Duration::from_secs(5))
    };

    fs::write(&release, b"release\n").unwrap();
    let first_result = first.join().expect("join named thread");
    let second_result = second.join().expect("join detached thread");
    let both_entered = wait_for_lines(&log, 2, Duration::from_secs(5));

    if same_common {
        assert!(
            !second_escaped,
            "same-process detached call escaped before the named Git process exited"
        );
        assert!(
            both_entered,
            "both same-process calls did not eventually enter Git"
        );
    } else {
        assert!(
            both_entered_before_release,
            "different common dirs were serialized by one process-global mutex"
        );
    }
    first_result.expect("named thread worktree add");
    second_result.expect("detached thread worktree add");
}

fn run_thread_scenario(label: &str, same_common: bool) {
    let temp = TempDir::new(label);
    let bin = install_fake_git(&temp);
    let caller_a = temp.join("thread-caller-a");
    let caller_b = temp.join("thread-caller-b");
    let common_a = temp.join("thread-common-a");
    let common_b = if same_common {
        common_a.clone()
    } else {
        temp.join("thread-common-b")
    };
    let active_a = temp.join("thread-active-a");
    let active_b = if same_common {
        active_a.clone()
    } else {
        temp.join("thread-active-b")
    };
    let log = temp.join("thread-entered.log");
    let release = temp.join("thread-release");
    for dir in [&caller_a, &caller_b, &common_a, &common_b] {
        fs::create_dir_all(dir).unwrap();
    }
    let common_a = fs::canonicalize(&common_a).unwrap();
    let common_b = fs::canonicalize(&common_b).unwrap();

    let output = Command::new(std::env::current_exe().expect("current test binary"))
        .args(["--exact", "worktree_lock_thread_worker", "--nocapture"])
        .env(
            "ORCH_B249_THREAD_MODE",
            if same_common {
                "same-common"
            } else {
                "different-common"
            },
        )
        .env("ORCH_FAKE_CALLER_A", &caller_a)
        .env("ORCH_FAKE_CALLER_B", &caller_b)
        .env("ORCH_FAKE_COMMON_DIR_A", &common_a)
        .env("ORCH_FAKE_COMMON_DIR_B", &common_b)
        .env("ORCH_FAKE_ACTIVE_A", &active_a)
        .env("ORCH_FAKE_ACTIVE_B", &active_b)
        .env("ORCH_FAKE_COMMON_DIR", &common_a)
        .env("ORCH_FAKE_ACTIVE", &active_a)
        .env("ORCH_FAKE_LOG", &log)
        .env("ORCH_FAKE_RELEASE", &release)
        .env("ORCH_FAKE_WORKER", "thread")
        .env("PATH", &bin)
        .stdin(Stdio::null())
        .output()
        .expect("run same-process thread worker");
    assert!(
        output.status.success(),
        "thread scenario {label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn same_process_threads_share_the_canonical_common_dir_lock() {
    run_thread_scenario("same-process-common", true);
}

#[test]
fn same_process_threads_keep_different_common_dirs_parallel() {
    run_thread_scenario("same-process-different", false);
}

#[test]
fn named_and_detached_processes_share_one_common_dir_lock() {
    let temp = TempDir::new("same-common");
    let bin = install_fake_git(&temp);
    let common = temp.join("common");
    let caller_a = temp.join("caller-a");
    let caller_b = temp.join("caller-b");
    let active = temp.join("active");
    let log = temp.join("entered.log");
    let release = temp.join("release");
    let ready_a = temp.join("ready-a");
    let ready_b = temp.join("ready-b");
    fs::create_dir_all(&common).unwrap();
    fs::create_dir_all(&caller_a).unwrap();
    fs::create_dir_all(&caller_b).unwrap();
    let common = fs::canonicalize(&common).unwrap();

    let mut first = spawn_worker(WorkerSpec {
        test_name: "worktree_lock_worker_named",
        caller: &caller_a,
        common: &common,
        active: &active,
        log: &log,
        release: &release,
        ready: &ready_a,
        bin: &bin,
        label: "named",
    });
    let first_entered = wait_for_lines(&log, 1, Duration::from_secs(5));
    if !first_entered {
        fs::write(&release, b"release\n").unwrap();
        let first_status = first.wait().unwrap();
        panic!("named worker never reached fake git; final status {first_status}");
    }
    let mut second = spawn_worker(WorkerSpec {
        test_name: "worktree_lock_worker_detached",
        caller: &caller_b,
        common: &common,
        active: &active,
        log: &log,
        release: &release,
        ready: &ready_b,
        bin: &bin,
        label: "detached",
    });

    let second_ready = wait_for_path(&ready_b, Duration::from_secs(5));
    let premature = poll_for_exit(&mut second, Duration::from_secs(1));
    let escaped_lock = premature.is_some();
    fs::write(&release, b"release\n").unwrap();
    let first_status = first.wait().unwrap();
    let second_status = match premature {
        Some(status) => status,
        None => second.wait().unwrap(),
    };
    let both_entered = wait_for_lines(&log, 2, Duration::from_secs(5));

    assert!(
        second_ready,
        "detached worker did not reach the production call readiness marker"
    );
    assert!(
        !escaped_lock,
        "detached worker reached fake git instead of blocking before process launch"
    );
    assert_success(first_status, "named");
    assert_success(second_status, "detached");
    assert!(
        both_entered,
        "timed out waiting for both workers to enter fake git"
    );
}

#[test]
fn different_common_dirs_do_not_share_a_global_lock() {
    let temp = TempDir::new("different-common");
    let bin = install_fake_git(&temp);
    let common_a = temp.join("common-a");
    let common_b = temp.join("common-b");
    let caller_a = temp.join("caller-a");
    let caller_b = temp.join("caller-b");
    let active_a = temp.join("active-a");
    let active_b = temp.join("active-b");
    let log = temp.join("entered.log");
    let release = temp.join("release");
    let ready_a = temp.join("ready-a");
    let ready_b = temp.join("ready-b");
    for dir in [&common_a, &common_b, &caller_a, &caller_b] {
        fs::create_dir_all(dir).unwrap();
    }
    let common_a = fs::canonicalize(&common_a).unwrap();
    let common_b = fs::canonicalize(&common_b).unwrap();

    let mut first = spawn_worker(WorkerSpec {
        test_name: "worktree_lock_worker_detached",
        caller: &caller_a,
        common: &common_a,
        active: &active_a,
        log: &log,
        release: &release,
        ready: &ready_a,
        bin: &bin,
        label: "repo-a",
    });
    let mut second = spawn_worker(WorkerSpec {
        test_name: "worktree_lock_worker_detached",
        caller: &caller_b,
        common: &common_b,
        active: &active_b,
        log: &log,
        release: &release,
        ready: &ready_b,
        bin: &bin,
        label: "repo-b",
    });
    let both_entered = wait_for_lines(&log, 2, Duration::from_secs(5));
    fs::write(&release, b"release\n").unwrap();
    let first_status = first.wait().unwrap();
    let second_status = second.wait().unwrap();

    assert!(
        both_entered,
        "different common dirs were serialized by one global lock"
    );
    assert_success(first_status, "repo-a");
    assert_success(second_status, "repo-b");
}
