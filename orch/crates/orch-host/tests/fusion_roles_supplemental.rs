//! Store concurrency, shared-worktree scope and the permanent default-feature gate.
use orch_host::fusion_roles::{load_config, save_config, FusionConfig};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
};
fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}
fn repo() -> PathBuf {
    let r = orch_host::util::test_scratch_dir("b359-store");
    git(&r, &["init", "-q"]);
    git(
        &r,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    );
    r
}
#[test]
fn empty_load_has_no_store_side_effect_and_save_adds_only_local_ignore() {
    let r = repo();
    let before = fs::read(r.join(".git/info/exclude")).unwrap();
    assert_eq!(load_config(&r).unwrap(), FusionConfig::default());
    assert!(!r.join(".orch").exists());
    save_config(&r, 0, &FusionConfig::default()).unwrap();
    let after = fs::read(r.join(".git/info/exclude")).unwrap();
    assert!(after.starts_with(&before));
    assert!(String::from_utf8(after)
        .unwrap()
        .contains("/.orch/fusion.json"));
    assert!(!r.join(".gitignore").exists());
}
#[test]
fn concurrent_save_never_silently_overwrites_a_revision() {
    let r = repo();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = vec![];
    for _ in 0..2 {
        let r = r.clone();
        let b = barrier.clone();
        handles.push(std::thread::spawn(move || {
            b.wait();
            save_config(&r, 0, &FusionConfig::default()).is_ok()
        }));
    }
    let winners = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(winners, 1);
    assert_eq!(load_config(&r).unwrap().revision, 1);
}
#[test]
fn linked_worktrees_share_roles_and_unrelated_repositories_do_not() {
    let r = repo();
    let linked = r.join("linked");
    git(
        &r,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "test-link",
            linked.to_str().unwrap(),
        ],
    );
    let saved = save_config(&linked, 0, &FusionConfig::default()).unwrap();
    assert_eq!(load_config(&r).unwrap(), saved);
    assert!(!linked.join(".orch").exists());
    let other = repo();
    assert_eq!(load_config(&other).unwrap().revision, 0);
    git(&r, &["worktree", "remove", linked.to_str().unwrap()]);
}
#[test]
fn symlink_leaf_and_oversize_config_are_not_read_or_overwritten() {
    use std::os::unix::fs::symlink;
    let r = repo();
    fs::create_dir(r.join(".orch")).unwrap();
    let outside = r.join("outside");
    fs::write(&outside, b"keep-me").unwrap();
    let config = r.join(".orch/fusion.json");
    symlink(&outside, &config).unwrap();
    assert!(load_config(&r).is_err());
    assert!(save_config(&r, 0, &FusionConfig::default()).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"keep-me");
    fs::remove_file(&config).unwrap();
    fs::write(config, vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert!(load_config(&r).is_err());
}
#[test]
fn native_parameters_do_not_accept_option_injection() {
    let mut tuple = orch_host::channel::InvocationTuple::default();
    tuple.model = Some("--dangerously-skip-permissions".into());
    assert!(orch_host::fusion_roles::validate_tuple(&tuple).is_err());
    tuple.model = Some("native/model-v-next".into());
    tuple.effort = Some("off".into());
    assert!(orch_host::fusion_roles::validate_tuple(&tuple).is_ok());
}
#[test]
fn exclusive_gate_really_invokes_default_host_lib() {
    let r = repo();
    let cargo = r.join("fake cargo");
    let calls = r.join("calls");
    fs::write(&cargo,r#"#!/bin/sh
printf '%s\n' "$*" >> "$B359_CALLS"
case " $* " in
  *" --list "*) last=''; for arg in "$@"; do [ "$arg" = '--' ] && break; last=$arg; done; printf '%s: test\n' "$last" ;;
esac
exit 0
"#).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let output = Command::new("/bin/sh")
        .arg(root.join("scripts/test-exclusive.sh"))
        .arg(cargo)
        .env("B359_CALLS", &calls)
        .current_dir(root.parent().unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = fs::read_to_string(calls).unwrap();
    assert!(logs.lines().any(|l| l.contains(
        "-p orch-host --lib --no-default-features --locked --manifest-path orch/Cargo.toml"
    )));
}

#[test]
fn native_role_guide_explains_real_boundaries() {
    let guide = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    for claim in [
        "## Native discovery and local Fusion role library",
        "expected-revision compare-and-swap",
        "model-version list",
        "Disabled aliases are not queried",
        "10 second command observation bound",
    ] {
        assert!(
            guide.contains(claim),
            "missing native role contract: {claim}"
        );
    }
}
#[test]
fn bare_repository_is_not_a_role_store() {
    let r = orch_host::util::test_scratch_dir("b359-bare");
    git(&r, &["init", "--bare", "-q"]);
    assert!(load_config(&r).is_err());
    assert!(save_config(&r, 0, &FusionConfig::default()).is_err());
    assert!(!r.join(".orch").exists());
}

#[test]
fn loading_rejects_symlink_directory_before_git_ignore_checks() {
    let r = repo();
    let outside = r.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(
        outside.join("fusion.json"),
        r#"{"version":1,"config":{"revision":7,"roles":[],"combinations":[]}}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, r.join(".orch")).unwrap();
    assert!(load_config(&r).is_err());
    assert!(!outside.join("fusion.lock").exists());
}

#[test]
fn fifo_config_is_rejected_without_waiting_for_a_writer() {
    let r = repo();
    fs::create_dir(r.join(".orch")).unwrap();
    assert!(Command::new("mkfifo")
        .arg(r.join(".orch/fusion.json"))
        .status()
        .unwrap()
        .success());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(load_config(&r).is_err());
    });
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)),
        Ok(true),
        "nonregular input must not block the reader"
    );
}
