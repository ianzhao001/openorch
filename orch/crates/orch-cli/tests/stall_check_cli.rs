#![cfg(feature = "selfhost")]
//! B247 real-CLI coverage for the read-only stall classifier.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct StallRepo {
    root: PathBuf,
    artifact_worktree: Option<PathBuf>,
    round: String,
}

impl Drop for StallRepo {
    fn drop(&mut self) {
        if let Some(worktree) = self.artifact_worktree.take() {
            let _ = fs::remove_dir_all(worktree);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeEntry {
    relative: PathBuf,
    kind: &'static str,
    len: u64,
    modified: SystemTime,
}

#[derive(Debug, PartialEq, Eq)]
struct ReadOnlyBaseline {
    status: Vec<u8>,
    refs: Vec<u8>,
    head: Vec<u8>,
    ledger: Vec<u8>,
    wal: Vec<u8>,
    runtime: Vec<RuntimeEntry>,
}

fn orch_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR should be <root>/orch/crates/orch-cli")
        .to_path_buf()
}

fn unique_fixture_path(label: &str) -> PathBuf {
    let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
    let parent = orch_dir().join("target/test-tmp");
    fs::create_dir_all(&parent).expect("create test-tmp");
    parent.join(format!(
        "b247-stall-{label}-{}-{serial}",
        std::process::id()
    ))
}

fn fixture_orch_command() -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, &[]);
    command
}

fn git(root: &Path, args: &[&str]) -> Output {
    support::fixture_git_command(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("launch git {args:?}: {error}"))
}

fn git_ok(root: &Path, args: &[&str]) -> Output {
    let output = git(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn commit_all(root: &Path, message: &str) {
    git_ok(root, &["add", "."]);
    let output = support::fixture_git_command(root)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_NAME", "B247 Test")
        .env("GIT_AUTHOR_EMAIL", "b247@example.invalid")
        .env("GIT_COMMITTER_NAME", "B247 Test")
        .env("GIT_COMMITTER_EMAIL", "b247@example.invalid")
        .output()
        .expect("launch fixture commit");
    assert!(
        output.status.success(),
        "fixture commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn event_line(
    event_id: &str,
    kind: &str,
    task: &str,
    round: &str,
    payload: serde_json::Value,
) -> String {
    serde_json::json!({
        "eventId": event_id,
        "ts": orch_host::ledger::now_rfc3339(),
        "actor": "runtime:orch",
        "type": kind,
        "taskId": task,
        "round": round,
        "payload": payload,
    })
    .to_string()
}

fn create_repo(label: &str, task: &str, terminal_kind: Option<&str>) -> StallRepo {
    let root = unique_fixture_path(label);
    fs::create_dir_all(&root).expect("create fixture root");
    git_ok(&root, &["init", "--quiet", "--initial-branch=main"]);

    let round = "rB247".to_string();
    let ledger_dir = root.join(format!("coordination/rounds/{round}"));
    let runtime = root.join("coordination/runtime");
    fs::create_dir_all(&ledger_dir).unwrap();
    fs::create_dir_all(runtime.join("ledger-wal")).unwrap();
    fs::create_dir_all(runtime.join("logs")).unwrap();
    fs::write(runtime.join("CURRENT-ROUND"), format!("{round}\n")).unwrap();
    fs::write(
        runtime.join("logs/baseline.marker"),
        b"read-only baseline\n",
    )
    .unwrap();

    let attempt = format!("{task}-A0001");
    let mut lines = vec![event_line(
        "evt-dispatch",
        "DispatchIssued",
        task,
        &round,
        serde_json::json!({
            "attemptId": attempt,
            "agent": "fixture-agent-no-process"
        }),
    )];
    if let Some(kind) = terminal_kind {
        lines.push(event_line(
            "evt-terminal",
            kind,
            task,
            &round,
            serde_json::json!({"attemptId": format!("{task}-A0001")}),
        ));
    }
    let ledger = format!("{}\n", lines.join("\n"));
    fs::write(ledger_dir.join("events.jsonl"), &ledger).unwrap();
    fs::write(runtime.join(format!("ledger-wal/{round}.jsonl")), &ledger).unwrap();
    commit_all(&root, "fixture baseline");

    StallRepo {
        root,
        artifact_worktree: None,
        round,
    }
}

fn add_current_report(repo: &mut StallRepo, task: &str) {
    let worktree = unique_fixture_path("artifact-worktree");
    let output = support::fixture_git_command(&repo.root)
        .args(["worktree", "add", "--quiet", "-b", &format!("task/{task}")])
        .arg(&worktree)
        .output()
        .expect("launch artifact worktree add");
    assert!(
        output.status.success(),
        "artifact worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report_dir = worktree.join(format!("coordination/rounds/{}/reports", repo.round));
    fs::create_dir_all(&report_dir).unwrap();
    fs::write(
        report_dir.join(format!("{task}-REPORT.md")),
        format!(
            "---\ntaskId: {task}\nattemptId: {task}-A0001\nagent: fixture-agent-no-process\n---\n"
        ),
    )
    .unwrap();
    commit_all(&worktree, "fixture current-attempt report");
    repo.artifact_worktree = Some(worktree);
}

fn run_stall_check(root: &Path) -> Output {
    fixture_orch_command()
        .arg("--root")
        .arg(root)
        .arg("stall-check")
        .output()
        .expect("run orch stall-check")
}

fn git_bytes(root: &Path, args: &[&str]) -> Vec<u8> {
    git_ok(root, args).stdout
}

fn runtime_manifest(root: &Path) -> Vec<RuntimeEntry> {
    fn visit(base: &Path, current: &Path, output: &mut Vec<RuntimeEntry>) {
        let mut entries = fs::read_dir(current)
            .expect("read runtime directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read runtime entry");
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).expect("stat runtime entry");
            let kind = if metadata.file_type().is_dir() {
                "dir"
            } else if metadata.file_type().is_file() {
                "file"
            } else if metadata.file_type().is_symlink() {
                "symlink"
            } else {
                "other"
            };
            output.push(RuntimeEntry {
                relative: path.strip_prefix(base).unwrap().to_path_buf(),
                kind,
                len: metadata.len(),
                modified: metadata.modified().expect("runtime mtime"),
            });
            if metadata.file_type().is_dir() {
                visit(base, &path, output);
            }
        }
    }

    let base = root.join("coordination/runtime");
    let mut output = Vec::new();
    visit(&base, &base, &mut output);
    output
}

fn baseline(repo: &StallRepo) -> ReadOnlyBaseline {
    ReadOnlyBaseline {
        status: git_bytes(
            &repo.root,
            &["status", "--porcelain=v2", "--untracked-files=all"],
        ),
        refs: git_bytes(
            &repo.root,
            &[
                "for-each-ref",
                "--sort=refname",
                "--format=%(refname)%00%(objectname)%00",
            ],
        ),
        head: git_bytes(&repo.root, &["symbolic-ref", "-q", "HEAD"]),
        ledger: fs::read(
            repo.root
                .join(format!("coordination/rounds/{}/events.jsonl", repo.round)),
        )
        .unwrap(),
        wal: fs::read(repo.root.join(format!(
            "coordination/runtime/ledger-wal/{}.jsonl",
            repo.round
        )))
        .unwrap(),
        runtime: runtime_manifest(&repo.root),
    }
}

#[test]
fn current_attempt_report_exits_one_and_preserves_the_full_baseline() {
    let task = "B900";
    let mut repo = create_repo("action", task, None);
    add_current_report(&mut repo, task);
    let before = baseline(&repo);
    assert!(before.status.is_empty(), "fixture must start clean");

    let output = run_stall_check(&repo.root);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("verdict=awaiting-collection"), "{stdout}");
    assert!(stdout.contains("needAction=true"), "{stdout}");
    assert_eq!(
        baseline(&repo),
        before,
        "stall-check must preserve porcelain-v2, every ref, HEAD, ledger/WAL bytes, and runtime names/sizes/mtimes"
    );
}

#[test]
fn recorded_task_exits_zero() {
    let repo = create_repo("quiet", "B901", Some("TaskRecorded"));

    let output = run_stall_check(&repo.root);

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("verdict=recorded"), "{stdout}");
    assert!(stdout.contains("needAction=false"), "{stdout}");
}

#[test]
fn missing_current_round_is_a_quiet_read_only_result() {
    let root = unique_fixture_path("no-round");
    fs::create_dir_all(&root).unwrap();

    let output = run_stall_check(&root);

    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("无活跃轮"));
    assert!(!root.join("coordination").exists());
    fs::remove_dir_all(root).unwrap();
}
