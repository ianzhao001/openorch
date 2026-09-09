//! B307 retained logical dirty-snapshot protection and exact-wake terminal reconciliation.
//! Task-resume generation/producer cases retired in B329; historical source and commits remain.

#![allow(dead_code)]

mod support_legacy_plan;
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::attempt::{self, AttemptRef};
use sha2::{Digest, Sha256};

const ROUND: &str = "r80-fixture";
const TASK: &str = "B307";
const AGENT: &str = "executor-desktop";

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git 启动失败");
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_status(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("git 启动失败")
        .success()
}

fn sha256(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}

struct SnapshotSite {
    root: PathBuf,
    worktree: PathBuf,
}

impl Drop for SnapshotSite {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn snapshot_site(tag: &str) -> SnapshotSite {
    let root = orch_host::util::test_scratch_dir(&format!("b307-snapshot-{tag}"));
    git(&root, &["init", "-q"]);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    fs::write(root.join(".gitignore"), ".worktrees/\n").unwrap();
    git(&root, &["add", "tracked.txt", ".gitignore"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git(&root, &["branch", "-M", "main"]);
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    let wt = root.join(".worktrees/B307");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B307",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(wt.join("tracked.txt"), "unstaged\n").unwrap();
    fs::write(wt.join("staged.txt"), "staged\n").unwrap();
    git(&wt, &["add", "staged.txt"]);
    fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
    SnapshotSite { root, worktree: wt }
}

#[test]
fn index_format_churn_keeps_a_logically_identical_dirty_snapshot_valid() {
    let site = snapshot_site("index-churn");
    git(&site.worktree, &["update-index", "--index-version=2"]);
    let index = PathBuf::from(git(&site.worktree, &["rev-parse", "--git-path", "index"]));
    let before_sha = sha256(&index);
    let before_tree = git(&site.worktree, &["write-tree"]);
    let before_status = git(&site.worktree, &["status", "--porcelain=v2"]);
    let observed = RefCell::new(None);
    let attempt = AttemptRef {
        task_id: TASK.into(),
        ordinal: 1,
        attempt_id: "B307-A0001".into(),
    };
    let snapshot = attempt::snapshot_worktree_wip_with_hook(
        &site.root,
        ROUND,
        &attempt,
        &mut |phase, _root, worktree| {
            if phase == "between-captures" {
                git(worktree, &["update-index", "--index-version=4"]);
                let after_sha = sha256(&index);
                let after_tree = git(worktree, &["write-tree"]);
                let after_status = git(worktree, &["status", "--porcelain=v2"]);
                *observed.borrow_mut() = Some((after_sha, after_tree, after_status));
            }
            Ok(())
        },
    )
    .expect("纯 index 编码变化不得阻塞逻辑等价的 WIP 快照")
    .expect("dirty worktree 必须产生 snapshot");
    let (after_sha, after_tree, after_status) = observed.into_inner().unwrap();
    assert_ne!(before_sha, after_sha, "测试必须真的改变 index 字节");
    assert_eq!(before_tree, after_tree, "stage tree 必须逻辑等价");
    assert_eq!(before_status, after_status, "porcelain 必须逻辑等价");
    assert!(snapshot.dirty);
    assert!(git_status(
        &site.root,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/{}", snapshot.archive_ref)
        ]
    ));
}

#[test]
fn content_change_still_refuses_and_publishes_no_archive() {
    let site = snapshot_site("content-change");
    let attempt = AttemptRef {
        task_id: TASK.into(),
        ordinal: 1,
        attempt_id: "B307-A0001".into(),
    };
    let result = attempt::snapshot_worktree_wip_with_hook(
        &site.root,
        ROUND,
        &attempt,
        &mut |phase, _root, worktree| {
            if phase == "between-captures" {
                fs::write(worktree.join("tracked.txt"), "changed again\n")?;
            }
            Ok(())
        },
    );
    assert!(result.is_err(), "逻辑内容变化仍必须 fail-closed");
    assert!(!git_status(
        &site.root,
        &[
            "rev-parse",
            "--verify",
            "refs/archive/r80-fixture-B307-A0001-wip",
        ]
    ));
}

#[test]
fn successor_terminal_reconcile_is_wake_scoped_and_idempotent() {
    let source = include_str!("../src/wake.rs");
    let reconcile = source
        .split_once("pub fn reconcile_pending_backend_receipts(")
        .unwrap()
        .1;
    assert!(reconcile.contains("payload_string(event, \"wakeId\") == Some(wake_id)"));
    assert!(reconcile.contains("ManagedWakeTerminated"));
    assert!(reconcile.contains("if !already_terminal"));
    assert!(reconcile.contains("terminal_capability_from_wake(wake)?"));
}
