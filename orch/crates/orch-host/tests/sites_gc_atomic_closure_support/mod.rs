//! Scratch-scene construction for the frozen B291 process-level contract.
//!
//! This module only creates repositories, durable lease facts, worktrees, and
//! measurement boundaries.  The frozen test obtains every verdict and every
//! reported field from a spawned `orch sites gc` process.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;

#[allow(dead_code)]
#[path = "../b289_fixture_round_isolation_support/mod.rs"]
mod synthetic_round;

/// Stable identity checked by the frozen carrier.
pub const CONTRACT_ID: &str = "B291";

/// One isolated repository and the paths observed by the frozen contract.
pub struct Scene {
    /// Canonical scratch repository root passed to the real CLI process.
    pub root: PathBuf,
    /// Active round copied from the signed fixture inputs.
    pub round: String,
    /// Exact worktree/target boundaries measured before and after GC.
    pub measurement_paths: Vec<PathBuf>,
    /// Stable repository metadata used to detect an illicit half-transition.
    pub registry_path: PathBuf,
    /// Site that must survive a refusal.
    pub protected_target: Option<PathBuf>,
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone)]
struct FixtureSite {
    task_id: String,
    attempt_id: String,
    role: &'static str,
    agent: String,
    site_id: String,
    generation: u32,
    reviewed_head: String,
    worktree: String,
    target: String,
}

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("orch-host manifest must live at <root>/orch/crates/orch-host")
        .to_path_buf()
}

/// Return the workspace-default binary required by the process-level seed.
pub fn orch_binary() -> PathBuf {
    source_root().join("orch/target/debug/orch")
}

/// Bind a spawned CLI to this scratch repository without relying on cwd.
pub fn configure_fixture_command(command: &mut Command, scene: &Scene) {
    command
        .arg("--root")
        .arg(&scene.root)
        .arg("--allow-stale-binary")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
}

fn run(command: &mut Command, label: &str) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{label} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "{label} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_git(root: &Path, arguments: &[&str], label: &str) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    run(&mut command, label);
}

fn git_stdout(root: &Path, arguments: &[&str], label: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap_or_else(|error| panic!("{label} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "{label} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|error| panic!("{label} produced non-UTF-8 stdout: {error}"))
        .trim()
        .to_string()
}

fn base_scene(tag: &str) -> Scene {
    let root = synthetic_round::synthesize_signed_round(&format!("b291-sites-gc-{tag}"));
    Scene {
        registry_path: root.join(".git/HEAD"),
        root,
        round: synthetic_round::SYNTHETIC_ROUND.to_string(),
        measurement_paths: Vec::new(),
        protected_target: None,
    }
}

fn append_event(scene: &Scene, event: &EventRecord) {
    let mut bytes = serde_json::to_vec(event).expect("serialize fixture event");
    bytes.push(b'\n');
    let ledger_path = scene
        .root
        .join(format!("coordination/rounds/{}/events.jsonl", scene.round));
    let mut ledger = OpenOptions::new()
        .append(true)
        .open(&ledger_path)
        .unwrap_or_else(|error| panic!("open {}: {error}", ledger_path.display()));
    ledger
        .write_all(&bytes)
        .expect("append fixture ledger event");

    let wal_path = scene.root.join(format!(
        "coordination/runtime/ledger-wal/{}.jsonl",
        scene.round
    ));
    let mut wal = OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .unwrap_or_else(|error| panic!("open {}: {error}", wal_path.display()));
    wal.write_all(&bytes).expect("append fixture WAL event");
}

fn append_lease(scene: &Scene, site: &FixtureSite) {
    append_event(
        scene,
        &orch_host::ledger::event(
            "WorkspaceLeased",
            "runtime:orch",
            Some(&site.task_id),
            Some(&scene.round),
            serde_json::json!({
                "siteId": site.site_id,
                "generation": site.generation,
                "attemptId": site.attempt_id,
                "role": site.role,
                "agent": site.agent,
                "reviewedHead": site.reviewed_head,
                "paths": {
                    "worktree": site.worktree,
                    "target": site.target,
                },
            }),
        ),
    );
}

fn append_release(scene: &Scene, site: &FixtureSite) {
    append_event(
        scene,
        &orch_host::ledger::event(
            "WorkspaceReleased",
            "runtime:orch",
            Some(&site.task_id),
            Some(&scene.round),
            serde_json::json!({
                "siteId": site.site_id,
                "generation": site.generation,
                "attemptId": site.attempt_id,
                "role": site.role,
                "agent": site.agent,
                "completionReceipt": "runtime:orch/managed-wake-terminated",
            }),
        ),
    );
}

fn review_site(scene: &Scene, index: usize) -> FixtureSite {
    let task_id = format!("BF{index:03}");
    let attempt_id = format!("{task_id}-A0001");
    let agent = format!("executor-fixture-{index}");
    let generation = 1;
    let basename = format!("review-{attempt_id}-primary-{agent}-g{generation:02}");
    FixtureSite {
        site_id: format!("{task_id}-primary-{agent}-g{generation:02}"),
        generation,
        task_id,
        attempt_id,
        role: "primary",
        agent,
        reviewed_head: git_stdout(&scene.root, &["rev-parse", "HEAD"], "resolve fixture HEAD"),
        worktree: format!(".worktrees/{basename}"),
        target: format!("orch/target/{basename}"),
    }
}

fn implement_site(scene: &Scene, generation: u32) -> FixtureSite {
    let task_id = "BFACTIVE".to_string();
    let attempt_id = "BFACTIVE-A0001".to_string();
    let agent = "executor-fixture-active".to_string();
    FixtureSite {
        site_id: format!("{task_id}-implement-{agent}-g{generation:02}"),
        generation,
        task_id: task_id.clone(),
        attempt_id,
        role: "implement",
        agent,
        reviewed_head: git_stdout(&scene.root, &["rev-parse", "HEAD"], "resolve fixture HEAD"),
        worktree: format!(".worktrees/{task_id}"),
        target: format!(".worktrees/{task_id}/orch/target"),
    }
}

fn register_worktree(scene: &Scene, site: &FixtureSite) -> PathBuf {
    let path = scene.root.join(&site.worktree);
    let path_text = path.to_string_lossy().into_owned();
    run_git(
        &scene.root,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            &path_text,
            "HEAD",
        ],
        "register fixture worktree",
    );
    path
}

fn disappear_worktree(path: &Path) {
    fs::remove_dir_all(path)
        .unwrap_or_else(|error| panic!("remove fixture worktree {}: {error}", path.display()));
}

fn sized_target(scene: &Scene, site: &FixtureSite, bytes: u64) -> PathBuf {
    let target = scene.root.join(&site.target);
    fs::create_dir_all(&target).expect("create sized fixture target");
    File::create(target.join("payload.bin"))
        .expect("create sized target payload")
        .set_len(bytes)
        .expect("size target payload");
    target
}

fn terminal_missing_site(scene: &mut Scene, index: usize, bytes: u64) -> FixtureSite {
    let site = review_site(scene, index);
    let worktree = register_worktree(scene, &site);
    let target = sized_target(scene, &site, bytes);
    append_lease(scene, &site);
    append_release(scene, &site);
    disappear_worktree(&worktree);
    scene.measurement_paths.push(worktree);
    scene.measurement_paths.push(target);
    site
}

/// Create multiple terminal generations whose missing worktrees are all Git-prunable.
pub fn scene_with_terminal_targets(tag: &str, count: usize) -> Scene {
    let mut scene = base_scene(tag);
    for index in 0..count {
        terminal_missing_site(&mut scene, index, 128 * 1024);
    }
    scene
}

/// Create one terminal generation with an exact known-size target.
pub fn scene_with_sized_target(tag: &str, known_bytes: u64) -> Scene {
    let mut scene = base_scene(tag);
    terminal_missing_site(&mut scene, 100, known_bytes);
    scene
}

/// Create a released generation sharing its canonical implementation path with an active lease.
pub fn scene_with_active_lease(tag: &str) -> Scene {
    let mut scene = base_scene(tag);
    let released = implement_site(&scene, 1);
    let active = implement_site(&scene, 2);
    let worktree = register_worktree(&scene, &released);
    let target = sized_target(&scene, &released, 64 * 1024);
    append_lease(&scene, &released);
    append_release(&scene, &released);
    append_lease(&scene, &active);
    scene.measurement_paths.push(worktree.clone());
    scene.protected_target = Some(target);
    scene
}

/// Create a target whose path is a regular file, so directory removal fails deterministically.
pub fn scene_with_undeletable_target(tag: &str) -> Scene {
    let mut scene = base_scene(tag);
    let site = review_site(&scene, 200);
    let worktree = register_worktree(&scene, &site);
    disappear_worktree(&worktree);
    let target = scene.root.join(&site.target);
    fs::create_dir_all(target.parent().expect("target parent")).expect("create target parent");
    fs::write(&target, b"not-a-directory\n").expect("create non-directory target");
    append_lease(&scene, &site);
    append_release(&scene, &site);
    scene.measurement_paths.push(worktree);
    scene.measurement_paths.push(target);
    scene
}

/// Create one exact released candidate plus an unrelated prunable registry entry.
pub fn scene_with_registry_prune_failure(tag: &str) -> Scene {
    let mut scene = base_scene(tag);
    terminal_missing_site(&mut scene, 300, 96 * 1024);

    let unrelated = scene
        .root
        .join(".worktrees/unrelated-prunable-registry-entry");
    let unrelated_text = unrelated.to_string_lossy().into_owned();
    run_git(
        &scene.root,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            &unrelated_text,
            "HEAD",
        ],
        "register unrelated prunable worktree",
    );
    disappear_worktree(&unrelated);
    scene
}
