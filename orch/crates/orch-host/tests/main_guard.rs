//! B162 seeded-red contract (H28②: the main checkout is guarded by machinery,
//! not by memory — and the guard never bites orch's own plumbing).
//!
//! Negative mutations that must turn the named case red:
//! M1. Allow a non-fast-forward move of refs/heads/main (or fail open when the
//!     ancestry is unknown) — that is exactly the r51/r53 ledger-wipe shape.
//! M2. Over-block: refuse planner fast-forward commits, other branches, or
//!     linked-worktree plumbing — a guard that strangles normal work will be
//!     uninstalled, which is worse than no guard.
//! M3. Ship a non-idempotent installer, a missing/non-executable hook script,
//!     or a violation record that is not one parseable JSON line.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::hooks::{ensure_main_guard, main_guard_verdict, violation_line, GuardVerdict};

static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root
        .join("target/test-tmp")
        .join(format!("main-guard-{name}-{}-{seq}", std::process::id()))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git").args(args).current_dir(dir).status().unwrap();
    assert!(status.success(), "git {args:?} must succeed in fixture");
}

#[test]
fn the_main_ref_never_moves_backwards_uninvited() {
    // M1: a non-fast-forward main update in the primary worktree is the wipe
    // shape; ancestry-unknown must also block (fail-closed).
    let non_ff = main_guard_verdict("refs/heads/main", Some(false), true, false);
    match non_ff {
        GuardVerdict::Block { reason } => {
            assert!(reason.contains("refs/heads/main"), "block names the ref: {reason}")
        }
        GuardVerdict::Allow => panic!("a non-fast-forward main move must be blocked"),
    }
    assert!(matches!(
        main_guard_verdict("refs/heads/main", None, true, false),
        GuardVerdict::Block { .. }
    ));
    // The stash ref in the primary worktree is the `git stash` shape.
    assert!(matches!(
        main_guard_verdict("refs/stash", Some(true), true, false),
        GuardVerdict::Block { .. }
    ));
}

#[test]
fn planner_commits_and_worktree_plumbing_stay_allowed() {
    // M2: fast-forward main (normal commits/merges), other refs, linked
    // worktrees, and orch's own bypass context all pass.
    assert!(matches!(
        main_guard_verdict("refs/heads/main", Some(true), true, false),
        GuardVerdict::Allow
    ));
    assert!(matches!(
        main_guard_verdict("refs/heads/task/B900", Some(false), true, false),
        GuardVerdict::Allow
    ));
    assert!(matches!(
        main_guard_verdict("refs/heads/main", Some(false), false, false),
        GuardVerdict::Allow
    ));
    assert!(matches!(
        main_guard_verdict("refs/heads/main", Some(false), true, true),
        GuardVerdict::Allow
    ));
}

#[test]
fn installation_is_idempotent_and_violations_are_one_json_line() {
    // M3: install twice, verify script + config, and the violation record is
    // machine-readable evidence, not prose.
    let root = temp_root("install");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    ensure_main_guard(&root).expect("first install succeeds");
    ensure_main_guard(&root).expect("second install is a no-op");
    let script = root.join(".githooks/reference-transaction");
    assert!(script.is_file(), "hook script is materialized");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "hook script is executable");
    }
    let configured = Command::new("git")
        .args(["config", "core.hooksPath"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&configured.stdout).trim(), ".githooks");

    let line = violation_line("refs/heads/main", "aaaa", "bbbb", "non-ff");
    let parsed: serde_json::Value = serde_json::from_str(&line).expect("one JSON line");
    assert_eq!(parsed["refname"], "refs/heads/main");
    assert_eq!(parsed["kind"], "non-ff");
    assert!(!line.contains('\n'), "exactly one line");
}
