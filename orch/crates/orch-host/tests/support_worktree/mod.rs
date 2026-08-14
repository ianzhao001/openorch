use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run_git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git should be available to the worktree test support")
}

/// Return only registered worktrees whose directory name starts with `prefix`.
pub fn scoped_worktrees(repo: &Path, prefix: &str) -> Vec<PathBuf> {
    let output = run_git(repo, &["worktree", "list", "--porcelain"]);
    assert!(
        output.status.success(),
        "git worktree list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .expect("git worktree list output should be utf-8")
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect()
}

/// Owns only the worktrees created through this guard.
#[allow(dead_code)]
pub struct ScopedWorktreeGuard {
    repo: PathBuf,
    prefix: String,
    owned: Vec<PathBuf>,
}

#[allow(dead_code)]
impl ScopedWorktreeGuard {
    pub fn new(repo: &Path, prefix: &str) -> Self {
        Self {
            repo: repo.to_path_buf(),
            prefix: prefix.to_string(),
            owned: Vec::new(),
        }
    }

    pub fn add(&mut self, name: &str) -> PathBuf {
        assert!(
            name.starts_with(&self.prefix),
            "guard-owned worktree name {name:?} must start with {:?}",
            self.prefix
        );
        assert_eq!(
            Path::new(name).file_name().and_then(|part| part.to_str()),
            Some(name),
            "guard-owned worktree name must be one path component"
        );

        let path = self.repo.join(".worktrees").join(name);
        std::fs::create_dir_all(
            path.parent()
                .expect("guard-owned worktree should have a parent directory"),
        )
        .expect("worktree parent should be creatable");
        let path_arg = path
            .to_str()
            .expect("worktree test path should be representable as utf-8");
        let output = run_git(
            &self.repo,
            &["worktree", "add", "--detach", path_arg, "HEAD"],
        );
        assert!(
            output.status.success(),
            "git worktree add failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        self.owned.push(path.clone());
        path
    }
}

impl Drop for ScopedWorktreeGuard {
    fn drop(&mut self) {
        for path in self.owned.iter().rev() {
            let Some(path_arg) = path.to_str() else {
                eprintln!(
                    "B196 worktree cleanup skipped non-utf8 owned path: {}",
                    path.display()
                );
                continue;
            };
            let output = run_git(&self.repo, &["worktree", "remove", "--force", path_arg]);
            if !output.status.success() {
                eprintln!(
                    "B196 worktree cleanup failed for {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        let output = run_git(&self.repo, &["worktree", "prune"]);
        if !output.status.success() {
            eprintln!(
                "B196 worktree prune failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
