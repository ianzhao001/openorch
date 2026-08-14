use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use orch_core::{read_ledger, EventRecord};
use orch_host::{ledger, plan, round, verify};

const IMPLEMENTER: &str = "executor-desktop";
const REVIEWER: &str = "executor-claw";

fn next_event_tick() {
    // `ledger::event` uses ULIDs. Keep every persisted fixture event in a
    // distinct millisecond so event IDs are strictly increasing even though
    // ULID randomness is otherwise unordered within one millisecond.
    std::thread::sleep(Duration::from_millis(2));
}

fn assert_monotonic(events: &[EventRecord]) {
    for pair in events.windows(2) {
        assert!(
            pair[0].event_id < pair[1].event_id,
            "fixture eventId order is not strictly monotonic: {} then {}",
            pair[0].event_id,
            pair[1].event_id
        );
        assert!(
            pair[0].ts <= pair[1].ts,
            "fixture timestamp order moved backwards: {} then {}",
            pair[0].ts,
            pair[1].ts
        );
    }
}

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("git must be available for the boundary audit fixture");
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
        .expect("git must be available for the boundary audit fixture");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output must be UTF-8")
        .trim()
        .to_string()
}

fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file must have a parent"))
            .expect("create fixture parent");
        fs::write(path, contents).expect("write fixture file");
        run_git(root, &["add", relative]);
    }
}

/// A real, repository-local Git fixture.  Every scratch repository lives below
/// the workspace's `orch/target/test-tmp`, never in the shared OS temp dir.
pub struct RepoFixture {
    root: PathBuf,
}

impl RepoFixture {
    pub fn new(tag: &str) -> Self {
        assert!(
            !tag.is_empty()
                && tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "fixture tag must be a non-empty path-safe token"
        );
        let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR must be below the orch workspace");
        let root = orch_root
            .join("target/test-tmp")
            .join(format!("b193-{tag}-{}", std::process::id()));
        assert!(
            !root.exists(),
            "boundary audit scratch unexpectedly exists: {}",
            root.display()
        );
        fs::create_dir_all(&root).expect("create repository-local scratch");
        run_git(&root, &["init", "-q"]);
        run_git(
            &root,
            &["config", "user.email", "b193-audit@example.invalid"],
        );
        run_git(&root, &["config", "user.name", "B193 audit fixture"]);
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn commit(&self, msg: &str, files: &[(&str, &str)]) -> String {
        write_files(&self.root, files);
        run_git(&self.root, &["commit", "-q", "-m", msg]);
        let sha = git_output(&self.root, &["rev-parse", "HEAD"]);
        if git_output(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"]) != "main" {
            run_git(&self.root, &["branch", "-M", "main"]);
        }
        sha
    }

    pub fn branch_commit(&self, branch: &str, base: &str, files: &[(&str, &str)]) -> String {
        run_git(&self.root, &["checkout", "-q", "-b", branch, base]);
        write_files(&self.root, files);
        run_git(&self.root, &["commit", "-q", "-m", "task change"]);
        let sha = git_output(&self.root, &["rev-parse", "HEAD"]);
        run_git(&self.root, &["checkout", "-q", "main"]);
        sha
    }

    pub fn merge_no_ff(&self, into: &str, head: &str) -> String {
        run_git(&self.root, &["checkout", "-q", into]);
        run_git(
            &self.root,
            &[
                "merge",
                "--no-ff",
                "--no-verify",
                "-q",
                head,
                "-m",
                "boundary audit merge",
            ],
        );
        git_output(&self.root, &["rev-parse", "HEAD"])
    }
}

impl Drop for RepoFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Construct a scenario marker using the same production event constructor as
/// the ledger.  `LedgerDraft::write` replaces these compact markers with the
/// complete production-built plan/dispatch/collect/verdict chain and writes the
/// returned events byte-for-byte to the canonical ledger and WAL paths.
pub fn event(actor: &str, kind: &str, task: Option<&str>) -> EventRecord {
    ledger::event(kind, actor, task, None, serde_json::json!({}))
}

pub struct LedgerDraft {
    round: String,
    task: String,
    markers: Vec<EventRecord>,
}

impl LedgerDraft {
    pub fn new(round: &str, task: &str) -> Self {
        Self {
            round: round.to_string(),
            task: task.to_string(),
            markers: Vec::new(),
        }
    }

    pub fn push(mut self, event: EventRecord) -> Self {
        self.markers.push(event);
        self
    }

    pub fn write(self, root: &Path) -> Vec<EventRecord> {
        assert!(
            self.markers
                .iter()
                .any(|marker| marker.kind == "PlanSignedOff"),
            "audit draft requires the signed-plan marker"
        );
        assert!(
            self.markers
                .iter()
                .any(|marker| marker.kind == "VerdictIssued"),
            "audit draft requires the root-verdict marker"
        );

        prepare_protocol_root(root, &self.round, &self.task);
        let task_head = git_output(root, &["rev-parse", &format!("task/{}", self.task)]);
        let base_sha = git_output(root, &["rev-parse", "main"]);
        let attempt = format!("{}-A0001", self.task);
        let go_path = format!(
            "coordination/rounds/{}/dispatch/{IMPLEMENTER}/GO-{}-A0001.md",
            self.round, self.task
        );

        next_event_tick();
        let dispatch = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(&self.task),
            Some(&self.round),
            serde_json::json!({
                "taskId": self.task,
                "agent": IMPLEMENTER,
                "attemptId": attempt,
                "attemptNo": 1,
                "goPath": go_path,
                "baseSha": base_sha,
            }),
        );
        ledger::append(root, &self.round, &[dispatch]).expect("append dispatch");

        next_event_tick();
        let receipt = ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some(&self.task),
            Some(&self.round),
            serde_json::json!({
                "actionId": format!("collect-{attempt}"),
                "attemptId": attempt,
                "attemptNo": 1,
                "agent": IMPLEMENTER,
                "baseSha": base_sha,
                "goPath": go_path,
                "branchSha": task_head,
            }),
        );
        let receipt_id = receipt.event_id.clone();
        ledger::append(root, &self.round, &[receipt]).expect("append collect receipt");

        next_event_tick();
        let collect = ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some(&self.task),
            Some(&self.round),
            serde_json::json!({
                "actionId": format!("collect-{attempt}"),
                "attemptId": attempt,
                "attemptNo": 1,
                "agent": IMPLEMENTER,
                "baseSha": base_sha,
                "goPath": go_path,
                "branchSha": task_head,
                "gateReceipt": receipt_id,
            }),
        );
        ledger::append(root, &self.round, &[collect]).expect("append collect completion");

        let review = root.join(format!(
            "coordination/rounds/{}/reviews/{}-A0001-primary-{REVIEWER}.md",
            self.round, self.task
        ));
        fs::write(
            review,
            format!(
                "---\ntaskId: {}\nround: {}\nattemptId: {}\nrole: primary\nreviewer: {REVIEWER}\nverdict: PASS\nreviewedHead: {task_head}\n---\nindependent fixture review\n",
                self.task, self.round, attempt
            ),
        )
        .expect("write review binding");
        fs::write(
            root.join(format!(
                "coordination/rounds/{}/evidence/{}-boundary-audit.json",
                self.round, self.task
            )),
            "{\"source\":\"b193-independent-audit\"}\n",
        )
        .expect("write evidence binding");

        run_git(root, &["add", "coordination"]);
        run_git(root, &["commit", "-q", "-m", "fixture signed contract"]);
        let expected_main = git_output(root, &["rev-parse", "main"]);
        next_event_tick();
        verify::run_root_verdict(
            root,
            &self.task,
            &attempt,
            &task_head,
            &expected_main,
            verify::RootVerdict::Pass,
            None,
            false,
        )
        .expect("produce canonical root verdict");

        let start_marker = self
            .markers
            .iter()
            .find(|marker| marker.kind == "MergeStarted");
        let Some(start_marker) = start_marker else {
            let events = ledger_events(root, &self.round);
            assert_monotonic(&events);
            return events;
        };

        let authorization = verify::validate_root_merge_authorization(
            root,
            &self.round,
            &self.task,
            &ledger_events(root, &self.round),
        )
        .expect("canonical fixture must authorize merge");
        let barrier_task = start_marker.task_id.as_deref().unwrap_or(&self.task);
        let barrier_actor = if start_marker.actor == "root" {
            "runtime:orch"
        } else {
            start_marker.actor.as_str()
        };
        next_event_tick();
        let started = ledger::event(
            "MergeStarted",
            barrier_actor,
            Some(barrier_task),
            Some(&self.round),
            serde_json::json!({
                "attemptId": authorization.attempt_id,
                "attemptNo": authorization.attempt_no,
                "headSha": authorization.head_sha,
                "mainHeadSha": authorization.main_head_sha,
                "collectCompletedEventId": authorization.collect_completed_event_id,
                "verdictEventId": authorization.verdict_event_id,
            }),
        );
        let mut events = ledger_events(root, &self.round);
        events.push(started);
        rewrite_ledger_and_wal(root, &self.round, &events);

        if barrier_actor != "runtime:orch" {
            assert_monotonic(&events);
            return events;
        }

        let mut board = OpenOptions::new()
            .append(true)
            .open(root.join("coordination/BOARD.md"))
            .expect("open fixture board");
        writeln!(board, "- independent audit first-parent advance").expect("append fixture board");
        run_git(root, &["add", "coordination/BOARD.md"]);
        run_git(
            root,
            &[
                "commit",
                "-q",
                "-m",
                "coordination-only first-parent advance",
            ],
        );
        run_git(
            root,
            &[
                "merge",
                "--no-ff",
                "--no-verify",
                "-q",
                &format!("task/{}", self.task),
                "-m",
                "independent boundary merge",
            ],
        );
        let merge_sha = git_output(root, &["rev-parse", "main"]);

        next_event_tick();
        let boundary = ledger::event(
            "EscalationRaised",
            "reviewer:orch-runtime",
            Some(barrier_task),
            Some(&self.round),
            serde_json::json!({
                "stage": "merge-boundary-shape",
                "mergeSha": serde_json::Value::Null,
                "actualMain": merge_sha,
                "actualHead": merge_sha,
                "reason": "independent audit replay of the H48 exact-parent rejection",
                "hint": "recover only after revalidating the complete authorization chain",
            }),
        );
        events.push(boundary);

        if self
            .markers
            .iter()
            .any(|marker| marker.kind == "TaskRecorded")
        {
            next_event_tick();
            events.push(ledger::event(
                "TaskRecorded",
                "runtime:orch",
                Some(&self.task),
                Some(&self.round),
                serde_json::json!({"postMergeGates": "all-green"}),
            ));
        }
        rewrite_ledger_and_wal(root, &self.round, &events);
        assert_monotonic(&events);
        events
    }
}

fn prepare_protocol_root(root: &Path, round_id: &str, task_id: &str) {
    // One seed case writes four independent ledger scenarios through the same
    // RepoFixture. Recreate only this PID-scoped scratch repository between
    // scenarios so no refs, worktrees, or untracked protocol bytes leak across
    // assertions. This intentionally does not use git reset/checkout recovery.
    for entry in fs::read_dir(root).expect("read protocol scratch") {
        let path = entry.expect("read protocol scratch entry").path();
        if path.is_dir() {
            fs::remove_dir_all(path).expect("remove prior protocol scratch directory");
        } else {
            fs::remove_file(path).expect("remove prior protocol scratch file");
        }
    }
    run_git(root, &["init", "-q"]);
    run_git(
        root,
        &["config", "user.email", "b193-audit@example.invalid"],
    );
    run_git(root, &["config", "user.name", "B193 audit fixture"]);
    fs::write(root.join("README.md"), "base\n").expect("write base README");
    fs::write(
        root.join(".gitignore"),
        ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
    )
    .expect("write fixture gitignore");
    run_git(root, &["add", "README.md", ".gitignore"]);
    run_git(root, &["commit", "-q", "-m", "base"]);
    run_git(root, &["branch", "-M", "main"]);
    run_git(root, &["checkout", "-q", "-b", &format!("task/{task_id}")]);
    fs::write(root.join("feature.txt"), "audited task change\n").expect("write task feature");
    run_git(root, &["add", "feature.txt"]);
    run_git(root, &["commit", "-q", "-m", "task change"]);
    run_git(root, &["checkout", "-q", "main"]);
    fs::create_dir_all(root.join(".worktrees")).expect("create worktree root");
    run_git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            &format!(".worktrees/{task_id}"),
            &format!("task/{task_id}"),
        ],
    );

    for directory in [
        "coordination/runtime",
        &format!("coordination/rounds/{round_id}/tasks"),
        &format!("coordination/rounds/{round_id}/reviews"),
        &format!("coordination/rounds/{round_id}/evidence"),
        "coordination/modes",
    ] {
        fs::create_dir_all(root.join(directory)).expect("create protocol fixture directory");
    }
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{round_id}\n"),
    )
    .expect("write current round");
    fs::write(
        root.join(format!(
            "coordination/rounds/{round_id}/tasks/{task_id}.md"
        )),
        format!(
            "---\ntaskId: {task_id}\nround: {round_id}\nagent: {IMPLEMENTER}\nseedProtocol: pure-spec\nentryPoints: [feature.txt]\nwriteSet: [feature.txt]\nfrozenPaths: [coordination/**]\ngates: {{fast: [postGate]}}\nbudgets: {{wallMinutes: 30}}\nrequiredReviews:\n  - {{role: primary, agent: {REVIEWER}}}\nrequiredEvidence: [boundary-audit]\n---\n# independent boundary audit fixture\n"
        ),
    )
    .expect("write fixture task card");
    fs::write(
        root.join("coordination/modes/test.yaml"),
        format!(
            "agents:\n  executor: {{adapter: test, tier: none}}\n  verifier: {{adapter: root-manual, tier: none}}\nhitl: {{mergeGate: auto}}\nverification: {{mode: root-manual-fixed-head}}\nliveness: {{monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}}\nscheduling:\n  allowedAgents: [{IMPLEMENTER}, {REVIEWER}]\n  capacities:\n    {IMPLEMENTER}: {{agent: 1, quota: 1, roles: [implement]}}\n    {REVIEWER}: {{agent: 1, quota: 1, roles: [primary-review]}}\nbudgets:\n  round: {{maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}}\ngit: {{pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}}\n"
        ),
    )
    .expect("write fixture mode");
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "project: {ecosystems: [test]}\nworkspace: {worktreeRoot: .worktrees}\nscope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  postGate:\n    argv: [\"/usr/bin/true\"]\n    timeoutSeconds: 30\n",
    )
    .expect("write fixture binding");
    fs::write(
        root.join("coordination/agents.yaml"),
        format!(
            "agents:\n  {IMPLEMENTER}:\n    injectable: true\n    sessionId: fixture\n    wake: {{argv: [\"/usr/bin/true\"]}}\n"
        ),
    )
    .expect("write fixture agents");
    fs::write(root.join("coordination/BOARD.md"), "# fixture board\n")
        .expect("write fixture board");

    let planned = plan::run_plan(root).expect("plan fixture round");
    assert!(planned.event_appended, "fixture plan must append its event");
    next_event_tick();
    round::run_sign_off(root, Some("B193 independent fixture sign-off"))
        .expect("sign off fixture round");
}

fn ledger_events(root: &Path, round_id: &str) -> Vec<EventRecord> {
    let read = read_ledger(&root.join(format!("coordination/rounds/{round_id}/events.jsonl")))
        .expect("read fixture ledger");
    assert!(
        read.bad_lines.is_empty(),
        "fixture ledger must have no bad lines"
    );
    read.events
}

fn rewrite_ledger_and_wal(root: &Path, round_id: &str, events: &[EventRecord]) {
    let mut bytes = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("serialize fixture event"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    bytes.push(b'\n');
    fs::write(
        root.join(format!("coordination/rounds/{round_id}/events.jsonl")),
        &bytes,
    )
    .expect("write canonical fixture ledger");
    let wal = root.join(format!("coordination/runtime/ledger-wal/{round_id}.jsonl"));
    fs::create_dir_all(wal.parent().expect("fixture WAL must have a parent"))
        .expect("create fixture WAL directory");
    fs::write(wal, bytes).expect("write fixture WAL mirror");
}
