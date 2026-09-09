#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use orch_core::{read_ledger, EventRecord};
use orch_host::{ledger, round, verify};


fn next_event_tick() {
    // `ledger::event` uses ULIDs. Keep every persisted fixture event in a
    // distinct millisecond for readable scenario boundaries. Event identity
    // uniqueness and ledger timestamp order are checked separately below;
    // ULID randomness is not a lexical ordering contract.
    std::thread::sleep(Duration::from_millis(2));
}

fn assert_monotonic(events: &[EventRecord]) {
    let identities = events.iter().map(|event| &event.event_id).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(identities.len(), events.len(), "duplicate fixture event identity");
    assert!(events.windows(2).all(|pair| pair[0].ts <= pair[1].ts));
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

        let verdict_task = self.markers.iter()
            .find(|marker| marker.kind == "VerdictIssued")
            .and_then(|marker| marker.task_id.as_deref())
            .unwrap_or(&self.task);
        prepare_protocol_root(root, &self.round, &self.task, verdict_task);
        let dispatched = orch_host::tierf::run_dispatch_local(root, verdict_task).unwrap();
        let attempt = dispatched.attempt_id;
        let worktree = root.join(dispatched.worktree_rel);
        let feature = format!("feature-{verdict_task}.txt");
        fs::write(worktree.join(&feature), "audited task change\n").unwrap();
        run_git(&worktree, &["add", &feature]);
        run_git(&worktree, &["commit", "-q", "-m", "task change"]);
        let implementation_head = git_output(&worktree, &["rev-parse", "HEAD"]);
        let report_rel = format!("coordination/rounds/{}/reports/{verdict_task}-REPORT.md", self.round);
        let wrote_at = ledger::now_rfc3339();
        fs::create_dir_all(worktree.join(&report_rel).parent().unwrap()).unwrap();
        fs::write(worktree.join(&report_rel), format!(
            "---\ntaskId: {verdict_task}\nagent: local\nbranch: task/{verdict_task}\nheadSha: {implementation_head}\nwroteAt: {wrote_at}\n---\n\
## 0 执行环境自报\nMODEL=fixture\nDEPTH=fixture\nCAPTURE=fixture\n\
## 1 变更文件清单\n{feature}\n\
## 2 提交序列\nimplementation then report\n\
## 3 种子搬运证据\nverify-only: no seed\n\
## 4 快门实测\ntrue gate\n\
## 5 负向变异自证\nindependent boundary fixture\n\
## 6 我可能做错的地方\nfixture only\n"
        )).unwrap();
        run_git(&worktree, &["add", &report_rel]);
        run_git(&worktree, &["commit", "-q", "-m", "report"]);
        let task_head = git_output(&worktree, &["rev-parse", "HEAD"]);
        assert!(matches!(orch_host::tierf::run_await(root, verdict_task, 30, None).unwrap(),
            orch_host::tierf::AwaitOutcome::Collected(_)));
        // Preserve real hooks through dispatch/collect; isolate only the
        // later synthetic crash-audit commits, not live task actions.
        let hooks = root.join(".git/boundary-fixture-hooks");
        fs::create_dir_all(&hooks).unwrap();
        run_git(root, &["config", "core.hooksPath", hooks.to_str().unwrap()]);
        fs::write(root.join(format!("coordination/rounds/{}/evidence/{verdict_task}-boundary-audit.json", self.round)),
            "{\"source\":\"b193-independent-audit\"}\n").unwrap();
        run_git(root, &["add", "coordination"]);
        run_git(root, &["commit", "-q", "-m", "bind collect and evidence"]);
        let expected_main = git_output(root, &["rev-parse", "main"]);
        verify::run_root_verdict(root, verdict_task, &attempt, &task_head, &expected_main,
            verify::RootVerdict::Pass, None, false).expect("produce schema3 root verdict");

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
            verdict_task,
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
                &format!("task/{verdict_task}"),
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

fn prepare_protocol_root(root: &Path, round_id: &str, task_id: &str, verdict_task: &str) {
    // Each scenario owns this repository-local scratch; keep no mutable state
    // from the preceding scenario.
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() { fs::remove_dir_all(path).unwrap(); }
        else { fs::remove_file(path).unwrap(); }
    }
    run_git(root, &["init", "-q", "-b", "main"]);
    run_git(root, &["config", "user.email", "b193-audit@example.invalid"]);
    run_git(root, &["config", "user.name", "B193 audit fixture"]);
    fs::write(root.join("README.md"), "base\n").unwrap();
    fs::write(root.join(".gitignore"),
        ".worktrees/\n.cowork-temp/\ncoordination/runtime/\ncoordination/rounds/*/dispatch/\norch/target/\n").unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let mut binding: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(source.join("coordination/PROJECT-BINDING.yaml")).unwrap()).unwrap();
    for command in binding["commands"].as_mapping_mut().unwrap().values_mut() {
        command["argv"] = serde_yaml::to_value(vec!["/usr/bin/true"]).unwrap();
        command["timeoutSeconds"] = serde_yaml::to_value(30_u64).unwrap();
    }
    fs::create_dir_all(root.join("coordination")).unwrap();
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), serde_yaml::to_string(&binding).unwrap()).unwrap();
    fs::write(root.join("coordination/BOARD.md"), "# fixture board\n").unwrap();
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-q", "-m", "fixture base"]);
    round::run_open_v3(root, round_id, "boundary audit", false).unwrap();
    for task in std::collections::BTreeSet::from([task_id, verdict_task]) {
        let path = root.join(format!("coordination/rounds/{round_id}/tasks/{task}.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!(
            "---\nschemaVersion: 3\ntaskId: {task}\nround: {round_id}\nseedProtocol: verify-only\nredForm: assertion\n\
dependsOn: []\nentryPoints: [feature-{task}.txt]\nseeds: []\nwriteSet: [feature-{task}.txt]\n\
frozenPaths: [coordination/rounds/**]\ngates: {{fast: [testFast]}}\nrequiredEvidence: [boundary-audit]\n---\n# boundary fixture\n")).unwrap();
        fs::write(root.join(format!("feature-{task}.txt")), "base\n").unwrap();
    }
    fs::create_dir_all(root.join(format!("coordination/rounds/{round_id}/evidence"))).unwrap();
    orch_host::plan::run_plan(root).unwrap();
    round::run_sign_off(root, Some("B193 schema3 fixture sign-off")).unwrap();
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-q", "-m", "signed fixture"]);
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

/// Minimal open schema-1/2 repository used only to prove that retained public
/// host writers reject before changing durable or Git state.
#[allow(dead_code)]
pub struct LegacyWriterFixture {
    root: PathBuf,
    round: String,
}

/// Byte and Git snapshot plus effect-directory presence used to prove that a
/// rejected legacy host action leaves its self-contained fixture unchanged.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWriterSnapshot {
    ledger: Vec<u8>,
    wal: Vec<u8>,
    head: String,
    status: String,
    worktrees: String,
    gate_logs_exist: bool,
    protocol_locks_exist: bool,
}

#[allow(dead_code)]
impl LegacyWriterFixture {
    /// Create a repository-local scratch repo with an open schema-1 round and
    /// matching ledger/WAL bytes, without invoking the rejected action itself.
    pub fn new(label: &str) -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("orch-host manifest must be below the workspace");
        let root = workspace.join("target/test-tmp").join(format!(
            "b323-legacy-writer-{label}-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        fs::create_dir_all(&root).expect("create legacy writer fixture");
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(
            &root,
            &["config", "user.email", "legacy-writer@example.invalid"],
        );
        run_git(&root, &["config", "user.name", "legacy writer fixture"]);
        fs::write(root.join("README.md"), "legacy writer fixture\n")
            .expect("write fixture baseline");
        fs::write(
            root.join(".gitignore"),
            "coordination/runtime/\ncoordination/rounds/*/events.jsonl\n.worktrees/\norch/target/\n",
        )
        .expect("write fixture ignore rules");
        run_git(&root, &["add", "README.md", ".gitignore"]);
        run_git(&root, &["commit", "-q", "-m", "fixture baseline"]);

        let round = "rLegacy".to_string();
        fs::create_dir_all(root.join(format!("coordination/rounds/{round}")))
            .expect("create legacy round");
        fs::create_dir_all(root.join("coordination/runtime/ledger-wal"))
            .expect("create legacy WAL root");
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{round}\n"),
        )
        .expect("write legacy current round");
        fs::write(
            root.join(format!("coordination/rounds/{round}/events.jsonl")),
            b"",
        )
        .expect("create legacy ledger");
        fs::write(
            root.join(format!("coordination/runtime/ledger-wal/{round}.jsonl")),
            b"",
        )
        .expect("create legacy WAL");
        ledger::append(
            &root,
            &round,
            &[ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some(&round),
                serde_json::json!({"purpose": "read-only legacy fixture"}),
            )],
        )
        .expect("append canonical legacy RoundOpened");
        Self { root, round }
    }

    /// Return the scratch repository root supplied to the host entry under test.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Return the fixture's deliberately legacy round identity.
    pub fn round(&self) -> &str {
        &self.round
    }

    /// Read the fixture's full HEAD for exact before/after comparison.
    pub fn head(&self) -> String {
        git_output(&self.root, &["rev-parse", "HEAD"])
    }

    /// Capture ledger/WAL bytes, HEAD/status/worktree registry and the presence
    /// of gate/lock directories; this does not manufacture a rejection fact.
    pub fn snapshot(&self) -> LegacyWriterSnapshot {
        LegacyWriterSnapshot {
            ledger: fs::read(
                self.root
                    .join(format!("coordination/rounds/{}/events.jsonl", self.round)),
            )
            .expect("read legacy ledger snapshot"),
            wal: fs::read(self.root.join(format!(
                "coordination/runtime/ledger-wal/{}.jsonl",
                self.round
            )))
            .expect("read legacy WAL snapshot"),
            head: self.head(),
            status: git_output(
                &self.root,
                &["status", "--porcelain=v2", "--untracked-files=all"],
            ),
            worktrees: git_output(&self.root, &["worktree", "list", "--porcelain"]),
            gate_logs_exist: self.root.join("coordination/runtime/gates").exists(),
            protocol_locks_exist: self.root.join("coordination/runtime/locks").exists(),
        }
    }

    /// Compare a fresh snapshot with the pre-action snapshot, including caches
    /// that would expose a lock or gate side effect despite unchanged events.
    pub fn assert_unchanged(&self, before: &LegacyWriterSnapshot) {
        assert_eq!(
            &self.snapshot(),
            before,
            "legacy writer changed fixture state"
        );
    }
}

impl Drop for LegacyWriterFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
