//! B306 non-seed regression for the repository-scoped full-gate capability.

use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orch_host::binding::CommandSpec;
use orch_host::gate::{
    reap_gate_process_group_with_control, register_gate_fixture,
    run_candidate_gate_with_permit_and_identity, run_full_gate_with_permits_and_identity,
    GateGroupMember, GateGroupObservation, GateReapControl, WorkspaceFullPermit,
    ORCH_GATE_FIXTURE_REGISTRY,
};
use orch_host::ledger::GateAuditIdentity;
use orch_host::storage::{GuardEntry, ProbeResult, StoragePermit, StorageThreshold};

const HELPER_MODE: &str = "ORCH_B306_FULL_PERMIT_HELPER";
const HELPER_ROOT: &str = "ORCH_B306_FULL_PERMIT_ROOT";
const HELPER_ATTEMPTED: &str = "ORCH_B306_FULL_PERMIT_ATTEMPTED";
const HELPER_ENTERED: &str = "ORCH_B306_FULL_PERMIT_ENTERED";
const HELPER_RELEASE: &str = "ORCH_B306_FULL_PERMIT_RELEASE";
const HELPER_DONE: &str = "ORCH_B306_FULL_PERMIT_DONE";
const FIXTURE_COMMAND: &str = "ORCH_B306_FULL_PERMIT_FIXTURE_COMMAND";
const GATE_SOURCE: &str = include_str!("../src/gate.rs");

fn unique_temp(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("host crate is below the orch workspace")
        .join("target/test-tmp")
        .join(format!(
            "b306-full-permit-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
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

fn setup_linked_repository() -> (PathBuf, PathBuf, PathBuf) {
    let base = unique_temp("repo");
    let root = base.join("main");
    let linked = base.join("linked");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    git(
        &root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    (base, root, linked)
}

fn common_dir(root: &Path) -> PathBuf {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let raw = String::from_utf8(output.stdout).unwrap();
    let path = PathBuf::from(raw.trim());
    fs::canonicalize(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
    .unwrap()
}

fn wait_file(path: &Path, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        path.exists(),
        "timed out waiting for {label}: {}",
        path.display()
    );
}

fn wait_owner_state(path: &Path, state: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(bytes) = fs::read(path) {
            if serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .is_some_and(|owner| owner["child"]["state"] == state)
            {
                return bytes;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for owner child state={state}");
}

fn spawn_helper(
    mode: &str,
    root: &Path,
    entered: &Path,
    release: Option<&Path>,
    done: Option<&Path>,
) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "workspace_full_permit_contract",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_MODE, mode)
        .env(HELPER_ROOT, root)
        .env(HELPER_ATTEMPTED, entered.with_extension("attempted"))
        .env(HELPER_ENTERED, entered)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(path) = release {
        command.env(HELPER_RELEASE, path);
    }
    if let Some(path) = done {
        command.env(HELPER_DONE, path);
    }
    command.spawn().unwrap()
}

fn run_helper(mode: &str) {
    let root = PathBuf::from(std::env::var_os(HELPER_ROOT).unwrap());
    let attempted = PathBuf::from(std::env::var_os(HELPER_ATTEMPTED).unwrap());
    let entered = PathBuf::from(std::env::var_os(HELPER_ENTERED).unwrap());
    fs::write(&attempted, b"attempted\n").unwrap();
    let gate_run_id = format!("helper{}", std::process::id());
    let mut permit =
        WorkspaceFullPermit::acquire(&root, &gate_run_id, "gate:B306:B306-A0001:helper-full")
            .unwrap();
    let spec = if mode == "crash" {
        CommandSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "printf child-ok".into()],
            timeout_seconds: 5,
            trial_timeout_seconds: None,
            approval: None,
        }
    } else {
        let release = PathBuf::from(std::env::var_os(HELPER_RELEASE).unwrap());
        CommandSpec {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'entered\\n' >\"$1\"; while [ ! -f \"$2\" ]; do /bin/sleep 0.02; done"
                    .into(),
                "b306-helper".into(),
                entered.display().to_string(),
                release.display().to_string(),
            ],
            timeout_seconds: 30,
            trial_timeout_seconds: None,
            approval: None,
        }
    };
    let storage = synthetic_storage_permit();
    let mut handle = permit.reentry_handle().unwrap();
    run_full_gate_with_permits_and_identity(
        &mut handle,
        &storage,
        GateAuditIdentity::Attempt {
            task_id: "B306",
            attempt_id: "B306-A0001",
        },
        "helper-full",
        &spec,
        &root,
        &root.join("coordination/runtime/logs"),
        &format!("helper-{gate_run_id}"),
    )
    .unwrap();
    drop(handle);
    if mode == "crash" {
        fs::write(&entered, b"converged\n").unwrap();
        std::mem::forget(permit);
        std::process::exit(0);
    }
    permit.release().unwrap();
    let done = PathBuf::from(std::env::var_os(HELPER_DONE).unwrap());
    fs::write(done, b"done\n").unwrap();
}

#[cfg(unix)]
fn run_registered_fixture_command() {
    let fixture = Command::new("/bin/sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = fixture.id();
    register_gate_fixture(
        std::env::var_os(ORCH_GATE_FIXTURE_REGISTRY).as_deref(),
        pgid,
        pgid,
    )
    .unwrap();
    std::mem::forget(fixture);
}

fn synthetic_storage_permit() -> StoragePermit {
    StoragePermit::acquire(
        &StorageThreshold::from_floor_bytes(0),
        GuardEntry::Gate,
        &ProbeResult::Ok {
            available_bytes: 1024,
            total_bytes: 2048,
        },
        0,
    )
    .unwrap()
}

fn quick_spec() -> CommandSpec {
    CommandSpec {
        argv: vec!["/bin/sh".into(), "-c".into(), "printf gate-ok".into()],
        timeout_seconds: 5,
        trial_timeout_seconds: None,
        approval: None,
    }
}

struct ReplacementBirthControl {
    pgid: u32,
    signaled: Vec<u32>,
}

impl GateReapControl for ReplacementBirthControl {
    fn observe_group(&mut self, pgid: u32) -> Result<GateGroupObservation, String> {
        assert_eq!(pgid, self.pgid);
        Ok(GateGroupObservation::Members(vec![GateGroupMember::live(
            pgid,
            "replacement-birth",
        )]))
    }

    fn signal_kill(&mut self, pgid: u32) -> std::io::Result<()> {
        self.signaled.push(pgid);
        Ok(())
    }

    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }

    fn sleep(&mut self, _duration: Duration) {}
}

#[test]
fn workspace_full_child_reaper_refuses_replacement_birth_before_signal() {
    let pgid = 91_306;
    let mut control = ReplacementBirthControl {
        pgid,
        signaled: Vec::new(),
    };
    let error = reap_gate_process_group_with_control(pgid, Some("owned-birth"), &mut control)
        .expect_err("a replacement process-group epoch must fail closed");
    assert!(format!("{error:#}").contains("birth mismatch"));
    assert!(
        control.signaled.is_empty(),
        "birth mismatch must be detected before the first signal"
    );
    assert!(GATE_SOURCE.contains("reap_workspace_full_process_group(lease.pgid, birth_identity)"));
}

#[test]
fn quick_exit_with_a_live_group_descendant_cannot_leave_spawning_owner_state() {
    let (base, root, _linked) = setup_linked_repository();
    let owner_path = common_dir(&root).join("orch-workspace-full-v1.owner.json");
    let mut permit = WorkspaceFullPermit::acquire(
        &root,
        "quick_exit_run",
        "gate:B306:B306-A0005:quick-exit-descendant",
    )
    .unwrap();
    let mut handle = permit.reentry_handle().unwrap();
    let spec = CommandSpec {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "/bin/sleep 0.4 & exit 0".into(),
        ],
        timeout_seconds: 5,
        trial_timeout_seconds: None,
        approval: None,
    };
    let result = run_full_gate_with_permits_and_identity(
        &mut handle,
        &synthetic_storage_permit(),
        GateAuditIdentity::Attempt {
            task_id: "B306",
            attempt_id: "B306-A0005",
        },
        "quick-exit-descendant",
        &spec,
        &root,
        &base.join("quick-exit-logs"),
        "quick-exit-descendant-quick_exit_run",
    );
    drop(handle);
    let release = permit.release();
    if result.is_err() || release.is_err() {
        std::thread::sleep(Duration::from_millis(500));
        let owner = fs::read_to_string(&owner_path).unwrap_or_default();
        let _ = fs::remove_file(&owner_path);
        let _ = fs::remove_dir_all(&base);
        panic!(
            "quick-exit cleanup failed: run={:?}; release={:?}; owner={owner}",
            result.err().map(|error| format!("{error:#}")),
            release.err().map(|error| format!("{error:#}"))
        );
    }
    assert!(!owner_path.exists());
    WorkspaceFullPermit::acquire(
        &root,
        "quick-exit-successor",
        "gate:B306:B306-A0005:quick-exit-successor",
    )
    .unwrap()
    .release()
    .unwrap();
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn workspace_full_permit_contract() {
    #[cfg(unix)]
    if std::env::var_os(FIXTURE_COMMAND).is_some() {
        run_registered_fixture_command();
        return;
    }
    if let Ok(mode) = std::env::var(HELPER_MODE) {
        run_helper(&mode);
        return;
    }

    let choke = GATE_SOURCE
        .split("fn run_gate_command(")
        .nth(1)
        .unwrap()
        .split("pub fn run_gate_guarded(")
        .next()
        .unwrap();
    assert!(
        choke
            .find("prepare_workspace_full_child_if_owned(")
            .unwrap()
            < choke.find(".spawn()").unwrap(),
        "durable spawning evidence must precede the child effect"
    );
    assert!(GATE_SOURCE.contains("WorkspaceFullChildStateV1::Spawning"));
    assert!(GATE_SOURCE.contains("WORKSPACE_FULL_EXECUTION.with"));

    let (base, root, linked) = setup_linked_repository();
    let first_entered = base.join("first-entered");
    let first_release = base.join("first-release");
    let first_done = base.join("first-done");
    let second_entered = base.join("second-entered");
    let second_release = base.join("second-release");
    let second_done = base.join("second-done");

    let mut first = spawn_helper(
        "hold",
        &root,
        &first_entered,
        Some(&first_release),
        Some(&first_done),
    );
    wait_file(&first_entered, "first full owner");
    let mut second = spawn_helper(
        "hold",
        &linked,
        &second_entered,
        Some(&second_release),
        Some(&second_done),
    );
    wait_file(
        &second_entered.with_extension("attempted"),
        "second full wrapper attempted acquisition",
    );
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        !second_entered.exists(),
        "linked worktree entered the same repo full lane concurrently"
    );

    // The long permit is not ledger.lock: an unrelated durable append can still
    // acquire that exact lock while another process owns the full lane.
    let ledger_lock_dir = root.join("coordination/runtime/locks");
    fs::create_dir_all(&ledger_lock_dir).unwrap();
    let ledger_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(ledger_lock_dir.join("ledger.lock"))
        .unwrap();
    let mut ledger_lock = fd_lock::RwLock::new(ledger_file);
    let ledger_guard = ledger_lock
        .try_write()
        .expect("full permit must not hold ledger.lock");
    drop(ledger_guard);

    // Candidate narrow execution remains independent of the occupied full lane.
    let candidate_root = linked.clone();
    let candidate_logs = base.join("candidate-logs");
    let (candidate_tx, candidate_rx) = mpsc::channel();
    let candidate = std::thread::spawn(move || {
        let permit = synthetic_storage_permit();
        let started = Instant::now();
        let result = run_candidate_gate_with_permit_and_identity(
            &permit,
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "candidate-check",
            &quick_spec(),
            &candidate_root,
            &candidate_logs,
            "candidate-narrow",
        )
        .map(|gate| (gate.exit_code, started.elapsed()))
        .map_err(|error| format!("{error:#}"));
        candidate_tx.send(result).unwrap();
    });
    let (candidate_exit, candidate_elapsed) = candidate_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("candidate narrow gate waited on the full permit")
        .expect("candidate narrow gate failed");
    assert_eq!(candidate_exit, 0);
    assert!(candidate_elapsed < Duration::from_secs(2));
    candidate.join().unwrap();

    fs::write(&first_release, b"release\n").unwrap();
    wait_file(&first_done, "first full release");
    assert!(first.wait().unwrap().success());
    wait_file(&second_entered, "second full owner after first release");
    fs::write(&second_release, b"release\n").unwrap();
    wait_file(&second_done, "second full release");
    assert!(second.wait().unwrap().success());

    let owner_path = common_dir(&root).join("orch-workspace-full-v1.owner.json");
    let other = base.join("other-repo");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "--quiet"]);
    git(
        &other,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "base",
        ],
    );
    {
        let mut permit =
            WorkspaceFullPermit::acquire(&root, "full", "gate:B306:B306-A0001:nested-full")
                .expect("parent acquires after both children");
        let exact_owner_bytes = fs::read(&owner_path).unwrap();
        let owner: serde_json::Value = serde_json::from_slice(&exact_owner_bytes).unwrap();
        assert_eq!(owner["gateRunId"], "full");
        assert_eq!(owner["pid"], std::process::id());
        assert_eq!(owner["action"], "gate:B306:B306-A0001:nested-full");
        assert!(owner["birthIdentity"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));

        let same_process_candidate = run_candidate_gate_with_permit_and_identity(
            &synthetic_storage_permit(),
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "same-process-candidate",
            &quick_spec(),
            &linked,
            &base.join("same-process-candidate-logs"),
            "same-process-candidate",
        )
        .unwrap();
        assert_eq!(same_process_candidate.exit_code, 0);
        let after_candidate: serde_json::Value =
            serde_json::from_slice(&fs::read(&owner_path).unwrap()).unwrap();
        assert_eq!(after_candidate["child"]["state"], "unspawned");

        std::env::set_var("ORCH_WORKSPACE_FULL_OWNER", owner.to_string());
        let forged = WorkspaceFullPermit::acquire(&linked, "forged", "test:forged")
            .expect_err("an env owner string must not authorize same-process reentry");
        std::env::remove_var("ORCH_WORKSPACE_FULL_OWNER");
        assert!(format!("{forged:#}").contains("reentry handle"));

        let storage = synthetic_storage_permit();
        for (field, drifted_value) in [
            ("gateRunId", "drifted-run"),
            ("action", "drifted-action"),
            ("birthIdentity", "drifted-owner-birth"),
        ] {
            let mut drifted = owner.clone();
            drifted[field] = serde_json::json!(drifted_value);
            fs::write(&owner_path, serde_json::to_vec(&drifted).unwrap()).unwrap();
            let drift_logs = base.join(format!("authority-drift-{field}"));
            let mut drift_handle = permit.reentry_handle().unwrap();
            let drift_error = match run_full_gate_with_permits_and_identity(
                &mut drift_handle,
                &storage,
                GateAuditIdentity::Attempt {
                    task_id: "B306",
                    attempt_id: "B306-A0001",
                },
                "nested-full",
                &quick_spec(),
                &linked,
                &drift_logs,
                "nested-full",
            ) {
                Err(error) => error,
                Ok(_) => panic!("durable owner authority drift authorized a spawn"),
            };
            drop(drift_handle);
            assert!(format!("{drift_error:#}").contains("authority"));
            assert!(!drift_logs.exists(), "authority drift created gate logs");
            fs::write(&owner_path, &exact_owner_bytes).unwrap();
        }

        let mut release_drift = owner.clone();
        release_drift["action"] = serde_json::json!("release-drift");
        fs::write(&owner_path, serde_json::to_vec(&release_drift).unwrap()).unwrap();
        let release_error = permit
            .release()
            .expect_err("failed release must remain visible and retriable");
        assert!(format!("{release_error:#}").contains("identity"));
        assert!(
            owner_path.exists(),
            "failed release removed the owner proof"
        );
        fs::write(&owner_path, &exact_owner_bytes).unwrap();

        let mut handle = permit.reentry_handle().unwrap();
        let cross_repo = match run_full_gate_with_permits_and_identity(
            &mut handle,
            &storage,
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "nested-full",
            &quick_spec(),
            &other,
            &base.join("cross-repo-logs"),
            "nested-full",
        ) {
            Err(error) => error,
            Ok(_) => panic!("a reentry handle must not cross repository identity"),
        };
        assert!(format!("{cross_repo:#}").contains("repo identity"));
        assert!(!base.join("cross-repo-logs").exists());

        let wrong_gate_id = match run_full_gate_with_permits_and_identity(
            &mut handle,
            &storage,
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "nested-full",
            &quick_spec(),
            &linked,
            &base.join("wrong-gate-id-logs"),
            "nested-wrongid",
        ) {
            Err(error) => error,
            Ok(_) => panic!("a reentry handle must bind gateRunId"),
        };
        assert!(format!("{wrong_gate_id:#}").contains("gateRunId/action"));
        assert!(!base.join("wrong-gate-id-logs").exists());

        let wrong_action = match run_full_gate_with_permits_and_identity(
            &mut handle,
            &storage,
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "different-full",
            &quick_spec(),
            &linked,
            &base.join("wrong-action-logs"),
            "nested-full",
        ) {
            Err(error) => error,
            Ok(_) => panic!("a reentry handle must bind gateRunId and action"),
        };
        assert!(format!("{wrong_action:#}").contains("gateRunId/action"));
        assert!(!base.join("wrong-action-logs").exists());

        let nested = run_full_gate_with_permits_and_identity(
            &mut handle,
            &storage,
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "nested-full",
            &quick_spec(),
            &linked,
            &base.join("nested-logs"),
            "nested-full",
        )
        .unwrap();
        assert_eq!(nested.exit_code, 0);

        let declared_registry = base.join("nested-logs/nested-full-gate-nested-full.fixtures");
        assert!(
            declared_registry.exists(),
            "full-owned spawn must durably pre-create its fixture registry"
        );
        fs::remove_file(&declared_registry).unwrap();
        let missing_registry_logs = base.join("missing-registry-logs");
        let missing_registry = match run_full_gate_with_permits_and_identity(
            &mut handle,
            &storage,
            GateAuditIdentity::Attempt {
                task_id: "B306",
                attempt_id: "B306-A0001",
            },
            "nested-full",
            &quick_spec(),
            &linked,
            &missing_registry_logs,
            "nested-full",
        ) {
            Err(error) => error,
            Ok(_) => panic!("a missing declared registry authorized another child"),
        };
        assert!(format!("{missing_registry:#}").contains("registry missing"));
        assert!(
            !missing_registry_logs.exists(),
            "missing prior registry must fail before the next log/spawn effect"
        );
        fs::write(&declared_registry, b"").unwrap();

        #[cfg(unix)]
        {
            let fixture_spec = CommandSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!(
                        "{}=1 exec \"$1\" --exact workspace_full_permit_contract --nocapture --test-threads=1",
                        FIXTURE_COMMAND
                    ),
                    "b306-fixture-gate".into(),
                    std::env::current_exe().unwrap().display().to_string(),
                ],
                timeout_seconds: 10,
                trial_timeout_seconds: None,
                approval: None,
            };
            let fixture_gate = run_full_gate_with_permits_and_identity(
                &mut handle,
                &storage,
                GateAuditIdentity::Attempt {
                    task_id: "B306",
                    attempt_id: "B306-A0001",
                },
                "nested-full",
                &fixture_spec,
                &linked,
                &base.join("nested-logs"),
                "nested-full",
            )
            .unwrap();
            assert_eq!(fixture_gate.exit_code, 0);
            let registry =
                fs::read_to_string(base.join("nested-logs/nested-full-gate-nested-full.fixtures"))
                    .unwrap();
            let pgid = registry.lines().next().unwrap().split('\t').nth(1).unwrap();
            assert!(
                !Command::new("/bin/kill")
                    .args(["-0", "--", &format!("-{pgid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap()
                    .success(),
                "registered cross-group fixture survived full-gate convergence"
            );
        }
    }
    assert!(
        !owner_path.exists(),
        "normal drop must release the owner record"
    );

    // A process which exits without Drop leaves a durable owner. Once its exact
    // PID/birth epoch is gone, the next owner reclaims it under the short lock.
    let crash_entered = base.join("crash-entered");
    let mut crashed = spawn_helper("crash", &linked, &crash_entered, None, None);
    wait_file(&crash_entered, "crashing full owner");
    assert!(crashed.wait().unwrap().success());
    assert!(
        owner_path.exists(),
        "crash fixture must leave stale owner evidence"
    );
    WorkspaceFullPermit::acquire(&root, "reclaimed", "test:reclaim")
        .expect("dead exact PID/birth owner must be reclaimed")
        .release()
        .unwrap();
    assert!(!owner_path.exists());

    // Killing only the orch owner must not release the repo while its exact
    // gate child group is still alive. A forged/stale child birth is an error,
    // and the next owner enters only after that exact group converges.
    let orphan_entered = base.join("orphan-entered");
    let orphan_release = base.join("orphan-release");
    let orphan_done = base.join("orphan-done");
    let mut orphan_owner = spawn_helper(
        "hold",
        &linked,
        &orphan_entered,
        Some(&orphan_release),
        Some(&orphan_done),
    );
    wait_file(&orphan_entered, "orphan gate child");
    let exact_owner = wait_owner_state(&owner_path, "running");
    let exact_json: serde_json::Value = serde_json::from_slice(&exact_owner).unwrap();
    assert_eq!(exact_json["child"]["state"], "running");
    orphan_owner.kill().unwrap();
    let _ = orphan_owner.wait().unwrap();

    let mut forged_json = exact_json.clone();
    forged_json["child"]["birthIdentity"] = serde_json::json!("forged-stale-birth");
    fs::write(&owner_path, serde_json::to_vec(&forged_json).unwrap()).unwrap();
    let stale = WorkspaceFullPermit::acquire(&root, "stale", "test:stale")
        .expect_err("stale child birth must refuse reclaim");
    assert!(
        format!("{stale:#}").contains("child birth mismatch"),
        "unexpected stale-birth error: {stale:#}"
    );
    fs::write(&owner_path, &exact_owner).unwrap();

    let (reclaim_tx, reclaim_rx) = mpsc::channel();
    let reclaim_root = root.clone();
    let reclaimer = std::thread::spawn(move || {
        let result = WorkspaceFullPermit::acquire(&reclaim_root, "afterchild", "test:after-child")
            .and_then(|mut permit| permit.release())
            .map_err(|error| format!("{error:#}"));
        reclaim_tx.send(result).unwrap();
    });
    assert!(
        reclaim_rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "dead parent alone released the full permit while its gate child lived"
    );
    fs::write(&orphan_release, b"release\n").unwrap();
    reclaim_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("reclaim did not complete after exact child convergence")
        .expect("reclaim after exact child convergence failed");
    reclaimer.join().unwrap();
    assert!(!owner_path.exists());

    fs::remove_dir_all(base).unwrap();
}
