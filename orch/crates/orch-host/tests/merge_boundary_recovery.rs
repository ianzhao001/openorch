//! H48 emergency regression coverage.
//!
//! The parent-shape tests use real git commits: the boundary proof is only as
//! strong as its ordered-parent and ancestry checks.  The recovery tests then
//! replay the B173 state (canonical root PASS + MergeStarted, a coordination-
//! only first-parent advance, a real no-ff merge, and the historical
//! `merge-boundary-shape` escalation) against the production recovery entry.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_core::{read_ledger, EventRecord};
use orch_host::{
    close::{run_merge_recovery, RecoveryOutcome},
    ledger, plan, round, run_merge, verify,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn scratch(label: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = orch_root.join("target/test-tmp").join(format!(
        "h48-{label}-{}-{}-{seq}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn commit_file(root: &Path, path: &str, contents: &str, message: &str) -> String {
    let absolute = root.join(path);
    fs::create_dir_all(absolute.parent().unwrap()).unwrap();
    fs::write(&absolute, contents).unwrap();
    git(root, &["add", path]);
    git(root, &["commit", "-q", "-m", message]);
    git_output(root, &["rev-parse", "HEAD"])
}

struct ShapeRepo {
    root: PathBuf,
    base: String,
    task_head: String,
}

impl ShapeRepo {
    fn new(label: &str) -> Self {
        let root = scratch(label);
        git(&root, &["init", "-q"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        git(&root, &["config", "user.name", "orch test"]);
        let base = commit_file(&root, "README.md", "base\n", "base");
        git(&root, &["branch", "-M", "main"]);
        git(&root, &["checkout", "-q", "-b", "task/T"]);
        let task_head = commit_file(&root, "feature.txt", "task\n", "task");
        git(&root, &["checkout", "-q", "main"]);
        Self {
            root,
            base,
            task_head,
        }
    }

    fn merge_task(&self) -> String {
        git(
            &self.root,
            &[
                "merge",
                "--no-ff",
                "--no-verify",
                "-q",
                "task/T",
                "-m",
                "merge task",
            ],
        );
        git_output(&self.root, &["rev-parse", "HEAD"])
    }
}

impl Drop for ShapeRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn coordination_only_first_parent_advance_has_a_complete_proof() {
    let repo = ShapeRepo::new("shape-coordination");
    let expected_main = repo.base.clone();
    let first_parent = commit_file(
        &repo.root,
        "coordination/BOARD.md",
        "planner note\n",
        "coordination advance",
    );
    let merge_sha = repo.merge_task();

    let proof = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &expected_main,
        &repo.task_head,
        &[],
    )
    .expect("coordination-only first-parent advance must be admitted");
    assert_eq!(proof.merge_sha, merge_sha);
    assert_eq!(proof.first_parent_sha, first_parent);
    assert_eq!(proof.task_head_sha, repo.task_head);
    assert_eq!(proof.expected_main_sha, expected_main);
    assert_eq!(proof.changed_paths, vec!["coordination/BOARD.md"]);
}

#[test]
fn wrong_second_parent_is_rejected() {
    let repo = ShapeRepo::new("shape-wrong-second");
    let expected_main = repo.base.clone();
    git(
        &repo.root,
        &["checkout", "-q", "-b", "other", &expected_main],
    );
    commit_file(&repo.root, "other.txt", "other\n", "other task");
    git(&repo.root, &["checkout", "-q", "main"]);
    git(
        &repo.root,
        &[
            "merge",
            "--no-ff",
            "--no-verify",
            "-q",
            "other",
            "-m",
            "wrong merge",
        ],
    );
    let merge_sha = git_output(&repo.root, &["rev-parse", "HEAD"]);

    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &expected_main,
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
    let repo = ShapeRepo::new("shape-three-parent");
    let expected_main = repo.base.clone();
    let first_parent = commit_file(
        &repo.root,
        "coordination/BOARD.md",
        "advance\n",
        "coordination advance",
    );
    git(
        &repo.root,
        &["checkout", "-q", "-b", "extra", &expected_main],
    );
    let extra = commit_file(&repo.root, "extra.txt", "extra\n", "extra parent");
    git(&repo.root, &["checkout", "-q", "main"]);
    let tree = git_output(
        &repo.root,
        &["rev-parse", &format!("{first_parent}^{{tree}}")],
    );
    let merge_sha = git_output(
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
        &expected_main,
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
    let repo = ShapeRepo::new("shape-nonancestor");
    let base = repo.base.clone();
    git(&repo.root, &["checkout", "-q", "-b", "verdict-main", &base]);
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
    let merge_sha = repo.merge_task();

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
    let repo = ShapeRepo::new("shape-orch-input");
    let expected_main = repo.base.clone();
    commit_file(
        &repo.root,
        "orch/crates/example/src/lib.rs",
        "pub fn changed() {}\n",
        "change compilation input",
    );
    let merge_sha = repo.merge_task();

    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &expected_main,
        &repo.task_head,
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("orch/crates/example/src/lib.rs"), "{error}");
}

#[test]
fn a_bound_review_or_evidence_change_is_rejected() {
    let repo = ShapeRepo::new("shape-bound-artifact");
    let expected_main = repo.base.clone();
    let bound = "coordination/rounds/r58/reviews/T-A0001-primary.md";
    commit_file(&repo.root, bound, "tampered\n", "tamper bound review");
    let merge_sha = repo.merge_task();

    let error = verify::validate_merge_commit_shape(
        &repo.root,
        &merge_sha,
        &expected_main,
        &repo.task_head,
        &[bound.to_string()],
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains(bound), "{error}");
}

const ROUND: &str = "rH48";
const TASK: &str = "BH48";
const ATTEMPT: &str = "BH48-A0001";

struct ProtocolFixture {
    root: PathBuf,
    expected_main: String,
}

impl ProtocolFixture {
    fn new(label: &str, red_after_merge: bool) -> Self {
        let root = scratch(label);
        git(&root, &["init", "-q"]);
        git(
            &root,
            &["config", "user.email", "orch-test@example.invalid"],
        );
        git(&root, &["config", "user.name", "orch test"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        git(&root, &["add", "README.md", ".gitignore"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);
        git(&root, &["checkout", "-q", "-b", "task/BH48"]);
        fs::write(root.join("feature.txt"), "merged\n").unwrap();
        git(&root, &["add", "feature.txt"]);
        git(&root, &["commit", "-q", "-m", "feature"]);
        git(&root, &["checkout", "-q", "main"]);
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        git(
            &root,
            &["worktree", "add", "-q", ".worktrees/BH48", "task/BH48"],
        );

        for directory in [
            "coordination/runtime",
            "coordination/rounds/rH48/tasks",
            "coordination/rounds/rH48/reviews",
            "coordination/rounds/rH48/evidence",
            "coordination/modes",
        ] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "rH48\n").unwrap();
        fs::write(
            root.join("coordination/rounds/rH48/tasks/BH48.md"),
            "---\n\
taskId: BH48\n\
round: rH48\n\
agent: executor-claw\n\
seedProtocol: pure-spec\n\
writeSet: [feature.txt]\n\
frozenPaths: [coordination/**]\n\
gates: {fast: [postGate]}\n\
budgets: {wallMinutes: 30}\n\
requiredReviews:\n\
  - {role: primary, agent: executor-desktop}\n\
requiredEvidence: [merge-boundary]\n\
---\n# fixture\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/modes/test.yaml"),
            r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement, primary-review]}
    executor-claw: {agent: 1, quota: 1, roles: [implement, primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [secondary-review]}
budgets:
  round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
        )
        .unwrap();
        let gate_command = if red_after_merge {
            "test \"$(git branch --show-current)\" = task/BH48"
        } else {
            "exit 0"
        };
        fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            format!(
                "project: {{ecosystems: [test]}}\nworkspace: {{worktreeRoot: .worktrees}}\nscope: {{protectedPaths: [\"coordination/**\"]}}\ngit: {{pushPolicy: forbidden}}\ncommands:\n  postGate:\n    argv: [\"sh\", \"-c\", {gate_command:?}]\n    timeoutSeconds: 30\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/agents.yaml"),
            "agents:\n  executor-claw:\n    injectable: true\n    sessionId: test\n    wake: {argv: [\"true\"]}\n",
        )
        .unwrap();
        fs::write(root.join("coordination/BOARD.md"), "# board\n").unwrap();

        let planned = plan::run_plan(&root).unwrap();
        assert!(planned.event_appended);
        round::run_sign_off(&root, Some("fixture root signoff")).unwrap();
        let task_head = git_output(&root, &["rev-parse", "task/BH48"]);
        let base_sha = git_output(&root, &["rev-parse", "main"]);
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "taskId": TASK,
                "agent": "executor-claw",
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "goPath": "coordination/rounds/rH48/dispatch/executor-claw/GO-BH48-A0001.md",
                "baseSha": base_sha,
            }),
        );
        let receipt = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "actionId": "collect-BH48-A0001",
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": base_sha,
                "goPath": "coordination/rounds/rH48/dispatch/executor-claw/GO-BH48-A0001.md",
                "branchSha": task_head,
            }),
        );
        let collect = ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "actionId": "collect-BH48-A0001",
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": base_sha,
                "goPath": "coordination/rounds/rH48/dispatch/executor-claw/GO-BH48-A0001.md",
                "branchSha": task_head,
                "gateReceipt": receipt.event_id,
            }),
        );
        ledger::append(&root, ROUND, &[dispatch, receipt, collect]).unwrap();
        fs::write(
            root.join("coordination/rounds/rH48/reviews/BH48-A0001-primary-executor-desktop.md"),
            format!(
                "---\ntaskId: BH48\nround: rH48\nattemptId: BH48-A0001\nrole: primary\nreviewer: executor-desktop\nverdict: PASS\nreviewedHead: {task_head}\n---\nfixture review\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/rH48/evidence/BH48-merge-boundary.json"),
            "{\"fixture\":true}\n",
        )
        .unwrap();
        git(&root, &["add", "coordination"]);
        git(&root, &["commit", "-q", "-m", "fixture signed contract"]);
        let expected_main = git_output(&root, &["rev-parse", "main"]);
        verify::run_root_verdict(
            &root,
            TASK,
            ATTEMPT,
            &task_head,
            &expected_main,
            verify::RootVerdict::Pass,
            None,
            false,
        )
        .unwrap();

        Self {
            root,
            expected_main,
        }
    }

    fn events(&self) -> Vec<EventRecord> {
        read_ledger(&self.root.join("coordination/rounds/rH48/events.jsonl"))
            .unwrap()
            .events
    }

    fn rewrite_ledger_and_wal(&self, events: &[EventRecord]) {
        let mut bytes = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        bytes.push(b'\n');
        fs::write(
            self.root.join("coordination/rounds/rH48/events.jsonl"),
            &bytes,
        )
        .unwrap();
        let wal = self.root.join("coordination/runtime/ledger-wal/rH48.jsonl");
        fs::create_dir_all(wal.parent().unwrap()).unwrap();
        fs::write(wal, bytes).unwrap();
    }

    /// Reproduce the exact H48/B173 historical shape without invoking the
    /// fixed implementation under test to manufacture its own fixture.
    fn replay_boundary_incident(&self) -> (String, String) {
        let authorization =
            verify::validate_root_merge_authorization(&self.root, ROUND, TASK, &self.events())
                .unwrap();
        let mut events = self.events();
        events.push(ledger::event(
            "MergeStarted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": authorization.attempt_id,
                "attemptNo": authorization.attempt_no,
                "headSha": authorization.head_sha,
                "mainHeadSha": authorization.main_head_sha,
                "collectCompletedEventId": authorization.collect_completed_event_id,
                "verdictEventId": authorization.verdict_event_id,
            }),
        ));
        self.rewrite_ledger_and_wal(&events);

        let mut board = OpenOptions::new()
            .append(true)
            .open(self.root.join("coordination/BOARD.md"))
            .unwrap();
        writeln!(board, "- prior merge bookkeeping").unwrap();
        git(&self.root, &["add", "coordination/BOARD.md"]);
        git(
            &self.root,
            &[
                "commit",
                "-q",
                "-m",
                "coordination-only first-parent advance",
            ],
        );
        let first_parent = git_output(&self.root, &["rev-parse", "main"]);
        git(
            &self.root,
            &[
                "merge",
                "--no-ff",
                "--no-verify",
                "-q",
                "task/BH48",
                "-m",
                "historical H48 merge",
            ],
        );
        let merge_sha = git_output(&self.root, &["rev-parse", "main"]);

        let mut events = self.events();
        events.push(ledger::event(
            "EscalationRaised",
            "reviewer:orch-runtime",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "stage": "merge-boundary-shape",
                "mergeSha": serde_json::Value::Null,
                "actualMain": merge_sha,
                "actualHead": merge_sha,
                "reason": "historical exact-parent check rejected coordination-only first-parent advance",
                "hint": "recover only after full boundary and authorization revalidation",
            }),
        ));
        self.rewrite_ledger_and_wal(&events);
        (first_parent, merge_sha)
    }
}

impl Drop for ProtocolFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn payload<'a>(event: &'a EventRecord, key: &str) -> Option<&'a serde_json::Value> {
    event.payload.as_ref()?.get(key)
}

fn stage(event: &EventRecord) -> Option<&str> {
    payload(event, "stage").and_then(serde_json::Value::as_str)
}

fn count_kind(events: &[EventRecord], kind: &str) -> usize {
    events.iter().filter(|event| event.kind == kind).count()
}

fn count_stage(events: &[EventRecord], expected: &str) -> usize {
    events
        .iter()
        .filter(|event| stage(event) == Some(expected))
        .count()
}

#[test]
fn b173_boundary_recovery_records_real_merge_and_is_idempotent() {
    let fixture = ProtocolFixture::new("recovery-green", false);
    let (first_parent, merge_sha) = fixture.replay_boundary_incident();
    assert_ne!(first_parent, fixture.expected_main);

    let outcome = run_merge_recovery(&fixture.root, TASK)
        .expect("valid B173 boundary incident must have a recovery exit");
    assert!(matches!(outcome, RecoveryOutcome { cleared: true }));
    let after_first = fixture.events();
    assert_eq!(count_kind(&after_first, "MergeExecuted"), 1);
    assert_eq!(count_kind(&after_first, "TaskRecorded"), 1);
    assert_eq!(count_stage(&after_first, "RecordGateRelaxed"), 1);
    let merged = after_first
        .iter()
        .find(|event| event.kind == "MergeExecuted")
        .unwrap();
    assert_eq!(
        payload(merged, "mergeSha").and_then(serde_json::Value::as_str),
        Some(merge_sha.as_str())
    );
    let relaxed = after_first
        .iter()
        .find(|event| stage(event) == Some("RecordGateRelaxed"))
        .unwrap();
    assert_eq!(
        payload(relaxed, "mergeSha").and_then(serde_json::Value::as_str),
        Some(merge_sha.as_str())
    );
    assert_eq!(
        payload(relaxed, "tipSha").and_then(serde_json::Value::as_str),
        Some(merge_sha.as_str())
    );
    assert_eq!(
        payload(relaxed, "files").and_then(serde_json::Value::as_array),
        Some(&Vec::new()),
        "tip==merge 时完整 diff 合法为空"
    );
    assert!(
        payload(relaxed, "reason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| reason.contains("H48")),
        "recovery relaxation must name H48"
    );
    assert_eq!(
        fs::read(fixture.root.join("coordination/rounds/rH48/events.jsonl")).unwrap(),
        fs::read(
            fixture
                .root
                .join("coordination/runtime/ledger-wal/rH48.jsonl")
        )
        .unwrap(),
        "ledger and WAL must remain byte-identical"
    );

    let first_counts = (
        count_kind(&after_first, "MergeExecuted"),
        count_kind(&after_first, "TaskRecorded"),
        count_stage(&after_first, "RecordGateRelaxed"),
    );
    let replay = run_merge_recovery(&fixture.root, TASK).expect("recovery replay is idempotent");
    assert!(replay.cleared);
    let after_replay = fixture.events();
    assert_eq!(
        (
            count_kind(&after_replay, "MergeExecuted"),
            count_kind(&after_replay, "TaskRecorded"),
            count_stage(&after_replay, "RecordGateRelaxed"),
        ),
        first_counts
    );
}

#[test]
fn b173_boundary_recovery_red_gate_records_truth_but_not_completion() {
    let fixture = ProtocolFixture::new("recovery-red", true);
    let (_, merge_sha) = fixture.replay_boundary_incident();

    let error = run_merge_recovery(&fixture.root, TASK)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("postGate") && (error.contains("红") || error.contains("exit")),
        "{error}"
    );
    let events = fixture.events();
    assert_eq!(count_kind(&events, "MergeExecuted"), 1);
    assert_eq!(count_kind(&events, "TaskRecorded"), 0);
    assert_eq!(count_stage(&events, "RecordGateRelaxed"), 0);
    let merged = events
        .iter()
        .find(|event| event.kind == "MergeExecuted")
        .unwrap();
    assert_eq!(
        payload(merged, "mergeSha").and_then(serde_json::Value::as_str),
        Some(merge_sha.as_str())
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| stage(event) == Some("post-merge-gate"))
            .count(),
        1
    );
    assert_eq!(
        fs::read(fixture.root.join("coordination/rounds/rH48/events.jsonl")).unwrap(),
        fs::read(
            fixture
                .root
                .join("coordination/runtime/ledger-wal/rH48.jsonl")
        )
        .unwrap(),
        "red recovery must mirror its truth events to WAL"
    );
}

#[test]
fn duplicate_canonical_boundary_escalations_are_not_recoverable() {
    let fixture = ProtocolFixture::new("recovery-duplicate-boundary", false);
    fixture.replay_boundary_incident();
    let mut events = fixture.events();
    let duplicate = events
        .iter()
        .find(|event| stage(event) == Some("merge-boundary-shape"))
        .unwrap()
        .clone();
    let mut duplicate = duplicate;
    duplicate.event_id = ulid::Ulid::new().to_string();
    events.push(duplicate);
    fixture.rewrite_ledger_and_wal(&events);

    let error = run_merge_recovery(&fixture.root, TASK)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("恰好一条") || error.contains("唯一") || error.contains("exactly one"),
        "{error}"
    );
    let events = fixture.events();
    assert_eq!(count_kind(&events, "MergeExecuted"), 0);
    assert_eq!(count_kind(&events, "TaskRecorded"), 0);
}

#[test]
fn unstaged_current_ledger_and_board_are_the_only_dirty_merge_allowlist() {
    let fixture = ProtocolFixture::new("board-allowlist", false);
    let mut board = OpenOptions::new()
        .append(true)
        .open(fixture.root.join("coordination/BOARD.md"))
        .unwrap();
    writeln!(board, "- human-readable bookkeeping pending commit").unwrap();

    run_merge(&fixture.root, TASK)
        .expect("unstaged current ledger + BOARD must not block an otherwise authorized merge");
    let events = fixture.events();
    assert_eq!(count_kind(&events, "MergeExecuted"), 1);
    assert_eq!(count_kind(&events, "TaskRecorded"), 1);
}

#[test]
fn staged_board_is_still_rejected_before_merge_started() {
    let fixture = ProtocolFixture::new("board-staged", false);
    let mut board = OpenOptions::new()
        .append(true)
        .open(fixture.root.join("coordination/BOARD.md"))
        .unwrap();
    writeln!(board, "- staged bookkeeping must not cross merge").unwrap();
    git(&fixture.root, &["add", "coordination/BOARD.md"]);

    let error = run_merge(&fixture.root, TASK).unwrap_err().to_string();
    assert!(
        error.contains("staged") || error.contains("dirty"),
        "{error}"
    );
    let events = fixture.events();
    assert_eq!(count_kind(&events, "MergeStarted"), 0);
    assert_eq!(count_kind(&events, "MergeExecuted"), 0);
}

#[test]
fn untracked_root_file_is_still_rejected_before_merge_started() {
    let fixture = ProtocolFixture::new("board-untracked", false);
    fs::write(fixture.root.join("untracked.txt"), "not allowlisted\n").unwrap();

    let error = run_merge(&fixture.root, TASK).unwrap_err().to_string();
    assert!(
        error.contains("untracked") || error.contains("staged") || error.contains("dirty"),
        "{error}"
    );
    let events = fixture.events();
    assert_eq!(count_kind(&events, "MergeStarted"), 0);
    assert_eq!(count_kind(&events, "MergeExecuted"), 0);
}
