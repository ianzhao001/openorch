//! Shared Git inspection must bind the supplied committed worktree.
//! Mutations: return the supplied subdirectory; skip HEAD verification; retain each
//! of GIT_DIR/GIT_WORK_TREE/GIT_CONFIG_PARAMETERS; bypass helper delegation.
use orch_host::gitx::canonical_committed_worktree;
use std::{fs, path::{Path, PathBuf}, process::Command};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null",
               "-c", "commit.gpgSign=false", "-c", "user.name=Fixture",
               "-c", "user.email=fixture@example.invalid"])
        .args(args).current_dir(root)
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_PARAMETERS").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
fn fixture() -> PathBuf {
    let root = orch_host::util::test_scratch_dir("committed worktree 中文");
    git(&root, &["init", "-q"]);
    git(&root, &["commit", "--allow-empty", "-qm", "base"]);
    root
}

#[test]
fn resolves_root_subdirectory_and_linked_worktree_without_collapsing_identity() {
    let root = fixture();
    let sub = root.join("nested 空格");
    fs::create_dir(&sub).unwrap();
    let expected = fs::canonicalize(&root).unwrap();
    assert_eq!(canonical_committed_worktree(&root).unwrap(), expected);
    assert_eq!(canonical_committed_worktree(&sub).unwrap(), expected);
    assert_eq!(orch_host::openorch_helper::canonical_project(&sub).unwrap(), expected);
    let linked = root.join("linked 作业");
    git(&root, &["worktree", "add", "--detach", linked.to_str().unwrap(), "HEAD"]);
    let linked = fs::canonicalize(linked).unwrap();
    assert_eq!(canonical_committed_worktree(&linked).unwrap(), linked);
    assert_eq!(orch_host::openorch_helper::canonical_project(&linked).unwrap(), linked);
    assert_ne!(linked, expected);
}

#[test]
fn rejects_missing_file_plain_bare_and_unborn_repositories() {
    if let Some(base) = std::env::var_os("ORCH_INVALID_WORKTREE_TEST_ROOT") {
        let base = PathBuf::from(base);
        for path in [&base, &base.join("missing"), &base.join("file"), &base.join("bare"), &base.join("unborn")] {
            assert!(canonical_committed_worktree(path).is_err(), "{}", path.display());
            assert!(orch_host::openorch_helper::canonical_project(path).is_err(), "{}", path.display());
        }
        return;
    }
    let base = orch_host::util::test_scratch_dir("invalid worktrees");
    let file = base.join("file");
    fs::write(&file, "not a directory").unwrap();
    let bare = base.join("bare");
    let unborn = base.join("unborn");
    fs::create_dir(&bare).unwrap();
    fs::create_dir(&unborn).unwrap();
    git(&bare, &["init", "--bare", "-q"]);
    git(&unborn, &["init", "-q"]);
    // Scratch stays in the managed target, but Git must not discover the outer repository.
    let base = fs::canonicalize(base).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "rejects_missing_file_plain_bare_and_unborn_repositories", "--nocapture"])
        .env("ORCH_INVALID_WORKTREE_TEST_ROOT", &base)
        .env("GIT_CEILING_DIRECTORIES", base.parent().unwrap())
        .output().unwrap();
    assert!(output.status.success(), "{} {}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn inherited_git_environment_cannot_redirect_inspection_or_helper() {
    // Re-enter this test in isolated processes; never mutate the test runner's environment.
    if let Some(root) = std::env::var_os("ORCH_INSPECTION_TEST_ROOT") {
        let root = PathBuf::from(root);
        assert_eq!(canonical_committed_worktree(&root).unwrap(), root);
        assert_eq!(orch_host::openorch_helper::canonical_project(&root).unwrap(), root);
        return;
    }
    let root = fs::canonicalize(fixture()).unwrap();
    for (key, value) in [
        ("GIT_DIR", root.join("missing-git-dir").to_str().unwrap().to_owned()),
        ("GIT_WORK_TREE", root.join("wrong-worktree").to_str().unwrap().to_owned()),
        ("GIT_CONFIG_PARAMETERS", "invalid unquoted config".to_owned()),
    ] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "inherited_git_environment_cannot_redirect_inspection_or_helper", "--nocapture"])
            .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_CONFIG_PARAMETERS")
            .env("ORCH_INSPECTION_TEST_ROOT", &root).env(key, value)
            .output().unwrap();
        assert!(output.status.success(), "{key}: {} {}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
}
