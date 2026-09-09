//! Pure merge-boundary proofs retained after the legacy recovery writer retires.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::{close, verify};

struct ShapeRepo {
    root: PathBuf,
    base: String,
    task_head: String,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit_file(root: &Path, path: &str, contents: &str, message: &str) -> String {
    let absolute = root.join(path);
    fs::create_dir_all(absolute.parent().unwrap()).unwrap();
    fs::write(absolute, contents).unwrap();
    git(root, &["add", path]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

impl ShapeRepo {
    fn new(label: &str) -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let root = workspace.join("target/test-tmp").join(format!(
            "b323-merge-shape-{label}-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "merge shape fixture"]);
        git(
            &root,
            &["config", "user.email", "merge-shape@example.invalid"],
        );
        let base = commit_file(&root, "README.md", "base\n", "base");
        git(&root, &["checkout", "-q", "-b", "task/T", &base]);
        let task_head = commit_file(&root, "feature.txt", "feature\n", "task");
        git(&root, &["checkout", "-q", "main"]);
        Self {
            root,
            base,
            task_head,
        }
    }

    fn merge(&self, branch: &str) -> String {
        git(
            &self.root,
            &[
                "merge",
                "--no-ff",
                "--no-verify",
                "-q",
                branch,
                "-m",
                "fixture merge",
            ],
        );
        git(&self.root, &["rev-parse", "HEAD"])
    }
}

impl Drop for ShapeRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn coordination_only_first_parent_advance_has_a_complete_proof() {
    let repo = ShapeRepo::new("coordination");
    let first_parent = commit_file(
        &repo.root,
        "coordination/BOARD.md",
        "planner note\n",
        "coordination advance",
    );
    let merge_sha = repo.merge("task/T");
    let proof = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &repo.base,
        &repo.task_head,
        &[],
    )
    .unwrap();
    assert_eq!(proof.first_parent_sha, first_parent);
    assert_eq!(proof.task_head_sha, repo.task_head);
    assert_eq!(proof.changed_paths, vec!["coordination/BOARD.md"]);
}

#[test]
fn wrong_second_parent_is_rejected() {
    let repo = ShapeRepo::new("wrong-parent");
    git(&repo.root, &["checkout", "-q", "-b", "other", &repo.base]);
    commit_file(&repo.root, "other.txt", "other\n", "other");
    git(&repo.root, &["checkout", "-q", "main"]);
    let merge_sha = repo.merge("other");
    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &repo.base,
        &repo.task_head,
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("second parent") || error.contains("第二"),
        "{error}"
    );
}

#[test]
fn octopus_merge_is_rejected_even_when_the_first_two_parents_match() {
    let repo = ShapeRepo::new("octopus");
    let first_parent = commit_file(
        &repo.root,
        "coordination/BOARD.md",
        "advance\n",
        "coordination advance",
    );
    git(&repo.root, &["checkout", "-q", "-b", "extra", &repo.base]);
    let extra = commit_file(&repo.root, "extra.txt", "extra\n", "extra parent");
    git(&repo.root, &["checkout", "-q", "main"]);
    let tree = git(
        &repo.root,
        &["rev-parse", &format!("{first_parent}^{{tree}}")],
    );
    let merge_sha = git(
        &repo.root,
        &[
            "commit-tree",
            &tree,
            "-p",
            &first_parent,
            "-p",
            &repo.task_head,
            "-p",
            &extra,
            "-m",
            "three parents",
        ],
    );
    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &repo.base,
        &repo.task_head,
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("two") || error.contains("两个") || error.contains("parent"),
        "{error}"
    );
}

#[test]
fn expected_main_must_be_an_ancestor_of_the_first_parent() {
    let repo = ShapeRepo::new("nonancestor");
    git(
        &repo.root,
        &["checkout", "-q", "-b", "verdict-main", &repo.base],
    );
    let forked_expected = commit_file(
        &repo.root,
        "coordination/fork.md",
        "fork\n",
        "forked verdict main",
    );
    git(&repo.root, &["checkout", "-q", "main"]);
    commit_file(
        &repo.root,
        "coordination/BOARD.md",
        "advance\n",
        "actual first parent",
    );
    let merge_sha = repo.merge("task/T");
    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &forked_expected,
        &repo.task_head,
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("祖先") || error.contains("ancestor"),
        "{error}"
    );
}

#[test]
fn compilation_input_in_the_first_parent_advance_is_rejected() {
    let repo = ShapeRepo::new("compilation-input");
    commit_file(
        &repo.root,
        "orch/crates/example/src/lib.rs",
        "pub fn changed() {}\n",
        "change compilation input",
    );
    let merge_sha = repo.merge("task/T");
    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &repo.base,
        &repo.task_head,
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("orch/crates/example/src/lib.rs"), "{error}");
}

#[test]
fn a_bound_review_or_evidence_change_is_rejected() {
    for (label, bound) in [
        (
            "bound-review",
            "coordination/rounds/r58/reviews/T-A0001-review-harness.md",
        ),
        (
            "bound-evidence",
            "coordination/rounds/r58/evidence/T-proof.json",
        ),
    ] {
        let repo = ShapeRepo::new(label);
        commit_file(&repo.root, bound, "tampered\n", "tamper bound artifact");
        let merge_sha = repo.merge("task/T");
        let error = verify::validate_merge_commit_shape(
            &repo.root,
            &merge_sha,
            &repo.base,
            &repo.task_head,
            &[bound.to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(bound), "{label}: {error}");
    }
}

#[test]
fn recovery_admission_remains_a_pure_fail_closed_table() {
    assert!(close::barrier_recovery_plan(true, false, true, false).is_ok());
    assert!(close::barrier_recovery_plan(true, true, true, true).is_ok());
    assert!(close::barrier_recovery_plan(false, false, true, false).is_err());
    assert!(close::barrier_recovery_plan(true, true, true, false).is_err());
    assert!(close::barrier_recovery_plan(true, false, false, true).is_err());
}
