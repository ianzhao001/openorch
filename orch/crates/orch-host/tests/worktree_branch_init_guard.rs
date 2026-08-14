//! H143 regression: the main guard must not misread a *branch* worktree
//! initialisation as a primary HEAD update.
//!
//! Defect (r69/B255-A0001, 2026-08-10): `initializing_linked_head_update` in
//! `.githooks/reference-transaction` compared the raw reference-transaction
//! value against the on-disk HEAD lock.  git reports a symbolic-ref transaction
//! as `ref:<target>` while the lock file stores git's file syntax
//! `ref: <target>` — one byte apart.  Both the length and the value comparison
//! therefore failed for every `git worktree add -b`, so the predicate returned
//! false and the guard blocked the creation as `primary-head`.
//!
//! It reproduced deterministically and had been doing so since the B231 hook
//! landed on 2026-08-06: `coordination/runtime/guard/violations.jsonl` holds
//! twenty symref-form `primary-head` records (`task/B233`, `task/B240`,
//! `task/B252`, `task/B255`).  Detached worktrees were unaffected because a raw
//! object id needs no normalisation, which is why every review site kept
//! working and the defect stayed invisible in the ledger.
//!
//! These cases drive the real `git worktree add` / `git checkout` commands
//! through an installed hook rather than feeding synthetic transaction lines,
//! so they fail if the wiring regresses and not merely if the predicate does.
//!
//! Negative mutations that must turn the named case red:
//! M1. Compare the raw transaction value again (drop the `ref:` normalisation)
//!     -> `branch_worktree_initialisation_is_allowed` red.
//! M2. Widen the predicate until it also accepts a primary HEAD update
//!     -> `primary_head_detach_is_still_blocked` red.
//! M3. Break detached initialisation while normalising
//!     -> `detached_worktree_initialisation_is_allowed` red.

use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::util::test_scratch_dir;

fn hook_source() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/orch/crates/orch-host
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("locate repo root")
        .join(".githooks")
}

fn git_raw(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=h143", "-c", "user.email=h143@test"])
        .args(args)
        .output()
        .expect("git invocation");
    let mut merged = String::from_utf8_lossy(&out.stdout).into_owned();
    merged.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), merged)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, output) = git_raw(dir, args);
    assert!(ok, "git {args:?} failed: {output}");
    output.trim().to_string()
}

/// A fixture repo with the production hook installed through `core.hooksPath`.
fn guarded_repo(tag: &str) -> (PathBuf, String) {
    let root = test_scratch_dir(tag);
    let repo = root.join("primary");
    std::fs::create_dir_all(&repo).expect("create fixture repo");
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["commit", "--allow-empty", "-m", "c1"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "config",
            "core.hooksPath",
            hook_source().to_str().expect("utf8 hook path"),
        ],
    );
    (repo, head)
}

#[test]
fn branch_worktree_initialisation_is_allowed() {
    // M1: this is the exact command every Tier F GO instructs an executor to
    // run.  Before the fix it died with
    // `BLOCK: HEAD update in primary worktree old=0{40} new=ref:refs/heads/...`.
    let (repo, head) = guarded_repo("h143-branch");
    let worktree = repo.parent().expect("fixture parent").join("branch-wt");
    let (ok, output) = git_raw(
        &repo,
        &[
            "worktree",
            "add",
            worktree.to_str().expect("utf8 worktree path"),
            "-b",
            "task/H143",
            &head,
        ],
    );
    assert!(
        ok,
        "branch worktree initialisation must pass the main guard: {output}"
    );
    assert!(worktree.join(".git").exists(), "worktree was not created");
}

#[test]
fn detached_worktree_initialisation_is_allowed() {
    // M3: review sites are detached worktrees; normalising the symref form must
    // not disturb the raw-object-id path that already worked.
    let (repo, head) = guarded_repo("h143-detached");
    let worktree = repo.parent().expect("fixture parent").join("detached-wt");
    let (ok, output) = git_raw(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().expect("utf8 worktree path"),
            &head,
        ],
    );
    assert!(
        ok,
        "detached worktree initialisation must keep passing: {output}"
    );
}

#[test]
fn primary_head_detach_is_still_blocked() {
    // M2: the guard's actual purpose (H118).  Loosening the initialisation
    // predicate must not let the primary worktree be checked out from under
    // the runtime.
    let (repo, head) = guarded_repo("h143-primary");
    let (ok, output) = git_raw(&repo, &["checkout", "--detach", &head]);
    assert!(
        !ok,
        "primary HEAD detach must stay blocked after the fix: {output}"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "primary worktree must still be on main"
    );
}
