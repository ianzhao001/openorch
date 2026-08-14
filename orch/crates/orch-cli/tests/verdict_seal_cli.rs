mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const ROUND: &str = "r904";
const TASK: &str = "B904";
const ATTEMPT: &str = "B904-A0001";
const REVIEWER: &str = "executor-desktop";
const REVIEW_REL: &str = "coordination/rounds/r904/reviews/B904-A0001-primary-executor-desktop.md";

static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn git_output(root: &Path, args: &[&str]) -> Output {
    let mut command = support::fixture_git_command(root);
    command
        .env_remove("ORCH_MAIN_GUARD_BYPASS")
        .env_remove("ORCH_MAIN_GUARD_CONTEXT")
        .args(args)
        .output()
        .expect("run fixture git")
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = git_output(root, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct SealFixture {
    root: PathBuf,
    task_head: String,
    main_head: String,
    first_dispatch_base: String,
}

impl Drop for SealFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl SealFixture {
    fn new(tag: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("target/test-tmp")
            .join(format!(
                "b204-seal-cli-{tag}-{}-{}",
                std::process::id(),
                ROOT_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "seal-cli@example.invalid"]);
        git(&root, &["config", "user.name", "seal cli test"]);
        fs::write(root.join("README.md"), "base\n").unwrap();
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\n.cowork-temp/\ncoordination/runtime/\n",
        )
        .unwrap();
        git(&root, &["add", "README.md", ".gitignore"]);
        git(&root, &["commit", "--quiet", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);

        git(&root, &["checkout", "--quiet", "-b", "task/B904"]);
        fs::write(root.join("feature.txt"), "sealed\n").unwrap();
        git(&root, &["add", "feature.txt"]);
        git(&root, &["commit", "--quiet", "-m", "task feature"]);
        let task_head = git(&root, &["rev-parse", "HEAD"]);
        git(&root, &["checkout", "--quiet", "main"]);
        fs::create_dir_all(root.join(".worktrees")).unwrap();
        git(
            &root,
            &["worktree", "add", "--quiet", ".worktrees/B904", "task/B904"],
        );

        for relative in [
            "coordination/runtime",
            "coordination/modes",
            "coordination/rounds/r904/tasks",
            "coordination/rounds/r904/reviews",
            "coordination/rounds/r904/evidence",
        ] {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{ROUND}\n"),
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
        fs::write(
            root.join("coordination/PROJECT-BINDING.yaml"),
            "project: {ecosystems: [test]}\nworkspace: {worktreeRoot: .worktrees}\nscope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  sealGate:\n    argv: [\"sh\", \"-c\", \"exit 0\"]\n    timeoutSeconds: 30\n",
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r904/tasks/B904.md"),
            "---\ntaskId: B904\nround: r904\nagent: executor-claw\nseedProtocol: pure-spec\nentryPoints: [feature.txt]\nwriteSet: [feature.txt]\nfrozenPaths: [coordination/**]\ngates: {fast: [sealGate]}\nbudgets: {wallMinutes: 30}\nrequiredReviews:\n  - {role: primary, agent: executor-desktop}\nrequiredEvidence: [seal]\n---\n# seal fixture\n",
        )
        .unwrap();
        fs::write(root.join("coordination/BOARD.md"), "# board\n").unwrap();
        fs::write(
            root.join("coordination/agents.yaml"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "agents": {
                    "executor-claw": {
                        "injectable": true,
                        "sessionId": "seal-test-session",
                        "wake": {"argv": ["sh", "-c", "exit 0", "{session}", "{message}"]}
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let planned = orch_host::plan::run_plan(&root).unwrap();
        assert!(planned.event_appended);
        orch_host::round::run_sign_off(&root, Some("seal fixture signoff")).unwrap();
        let dispatch_base = git(&root, &["rev-parse", "main"]);
        let dispatch = orch_host::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "taskId": TASK,
                "agent": "executor-claw",
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "goPath": "coordination/rounds/r904/dispatch/executor-claw/GO-B904-A0001.md",
                "baseSha": dispatch_base,
            }),
        );
        let receipt = orch_host::ledger::event(
            "CollectGateSuccessReceipt",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "actionId": "collect-B904-A0001",
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "agent": "executor-claw",
                "baseSha": dispatch_base,
                "goPath": "coordination/rounds/r904/dispatch/executor-claw/GO-B904-A0001.md",
                "branchSha": task_head,
            }),
        );
        let durable_collect = serde_json::json!({
            "actionId": "collect-B904-A0001",
            "attemptId": ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-claw",
            "baseSha": dispatch_base,
            "goPath": "coordination/rounds/r904/dispatch/executor-claw/GO-B904-A0001.md",
            "owner": "seal-fixture-owner",
            "leaseGeneration": "seal-fixture-generation",
        });
        let collect_claimed = orch_host::ledger::event(
            "ReportCollectClaimed",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            durable_collect.clone(),
        );
        let collect_executing = orch_host::ledger::event(
            "ReportCollectExecuting",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            durable_collect.clone(),
        );
        let mut executed_payload = durable_collect.clone();
        executed_payload["branchSha"] = serde_json::json!(task_head);
        let collect_executed = orch_host::ledger::event(
            "ReportCollectExecuted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            executed_payload,
        );
        let mut completed_payload = durable_collect;
        completed_payload["branchSha"] = serde_json::json!(task_head);
        completed_payload["evidenceSha256"] = serde_json::json!("e".repeat(64));
        completed_payload["evidenceLen"] = serde_json::json!(1);
        completed_payload["controlEpoch"] = serde_json::json!(dispatch.event_id);
        completed_payload["gateReceipt"] = serde_json::json!(receipt.event_id);
        let collect = orch_host::ledger::event(
            "ReportCollectCompleted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            completed_payload,
        );
        let review_requested = orch_host::ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": ATTEMPT,
                "role": "primary",
                "agent": "executor-desktop",
                "deadlineSecs": 1800,
                "requestedAt": "2026-08-02T00:00:00Z",
                "reviewedHead": task_head,
            }),
        );
        orch_host::ledger::append(
            &root,
            ROUND,
            &[
                dispatch,
                receipt,
                collect_claimed,
                collect_executing,
                collect_executed,
                collect,
                review_requested,
            ],
        )
        .unwrap();
        fs::write(
            root.join(REVIEW_REL),
            format!(
                "---\ntaskId: {TASK}\nround: {ROUND}\nattemptId: {ATTEMPT}\nrole: primary\nreviewer: executor-desktop\nverdict: PASS\nreviewedHead: {task_head}\n---\nsubstantive seal fixture review\n"
            ),
        )
        .unwrap();
        fs::write(
            root.join("coordination/rounds/r904/evidence/B904-seal.json"),
            "{\"sealed\":true}\n",
        )
        .unwrap();

        // Install the exact tracked production hook before the signed main
        // commit. There is no root PASS yet, so this ordinary fast-forward is
        // expected to remain legal.
        orch_host::hooks::ensure_main_guard(&root).unwrap();
        git(&root, &["add", "coordination", ".githooks"]);
        git(&root, &["commit", "--quiet", "-m", "signed seal contract"]);
        let main_head = git(&root, &["rev-parse", "main"]);

        Self {
            root,
            task_head,
            main_head,
            first_dispatch_base: dispatch_base,
        }
    }

    fn orch(&self, args: &[&str]) -> Output {
        fixture_orch_command(&[])
            .arg("--root")
            .arg(&self.root)
            .args(args)
            .output()
            .expect("run Cargo-built orch")
    }

    fn events(&self) -> Vec<orch_core::EventRecord> {
        orch_core::read_ledger(&self.root.join("coordination/rounds/r904/events.jsonl"))
            .unwrap()
            .events
    }

    fn root_review_binding(&self) -> orch_host::verify::ReviewBinding {
        let event = self
            .events()
            .into_iter()
            .find(|event| {
                event.kind == "VerdictIssued"
                    && event.actor == "verifier:root"
                    && event.task_id.as_deref() == Some(TASK)
            })
            .expect("fixture root verdict");
        let payload: orch_host::verify::RootVerdictPayload =
            serde_json::from_value(event.payload.unwrap()).unwrap();
        let [binding] = payload.reviews.as_slice() else {
            panic!("fixture root verdict must bind exactly one review");
        };
        binding.clone()
    }

    fn write_initial_go(&self) {
        let path = self
            .root
            .join("coordination/rounds/r904/dispatch/executor-claw/GO-B904-A0001.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "# ignored fixture GO for B904-A0001\n").unwrap();
    }

    fn stage_review_inbox(&self, body: &str) {
        let rel =
            orch_host::wake::review_inbox_relpath(ROUND, ATTEMPT, "primary", REVIEWER).unwrap();
        let path = self.root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!(
                "---\ntaskId: {TASK}\nround: {ROUND}\nattemptId: {ATTEMPT}\nrole: primary\nreviewer: executor-desktop\nverdict: PASS\nreviewedHead: {}\n---\n{body}\n",
                self.task_head
            ),
        )
        .unwrap();
    }

    fn assert_complete_chain(&self) {
        let events = self.events();
        orch_host::close::validate_seal_postcondition(&events, ROUND, TASK, ATTEMPT).unwrap();
        for kind in [
            "VerdictIssued",
            "MergeStarted",
            "MergeExecuted",
            "TaskRecorded",
        ] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| {
                        event.kind == kind && event.task_id.as_deref() == Some(TASK)
                    })
                    .count(),
                1,
                "{kind} must appear exactly once"
            );
        }
        assert_eq!(
            fs::read(self.root.join("coordination/rounds/r904/events.jsonl")).unwrap(),
            fs::read(self.root.join("coordination/runtime/ledger-wal/r904.jsonl")).unwrap(),
            "seal success requires exact ledger/WAL mirroring"
        );
        let main = git(&self.root, &["rev-parse", "main"]);
        let parents = git(&self.root, &["rev-list", "--parents", "-n", "1", &main]);
        let parents = parents.split_whitespace().collect::<Vec<_>>();
        assert_eq!(parents.len(), 3, "seal must create one no-ff merge commit");
        assert_eq!(parents[1], self.main_head);
        assert_eq!(parents[2], self.task_head);
    }
}

#[test]
fn seal_cli_captures_main_merges_records_and_replays_without_expected_main() {
    let fixture = SealFixture::new("one-command");

    let wrong = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        "0000000000000000000000000000000000000000",
    ]);
    assert!(!wrong.status.success());
    assert!(!String::from_utf8_lossy(&wrong.stdout).contains("complete"));
    assert!(!String::from_utf8_lossy(&wrong.stderr).contains("complete"));
    assert!(!fixture
        .events()
        .iter()
        .any(|event| event.kind == "VerdictIssued"));

    let sealed = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        sealed.status.success(),
        "seal stdout={} stderr={}",
        String::from_utf8_lossy(&sealed.stdout),
        String::from_utf8_lossy(&sealed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&sealed.stdout).contains("orch seal B904: complete"),
        "{}",
        String::from_utf8_lossy(&sealed.stdout)
    );
    fixture.assert_complete_chain();

    let ledger_before =
        fs::read(fixture.root.join("coordination/rounds/r904/events.jsonl")).unwrap();
    let replay = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(replay.status.success());
    assert!(String::from_utf8_lossy(&replay.stdout).contains("durable replay"));
    assert_eq!(
        fs::read(fixture.root.join("coordination/rounds/r904/events.jsonl")).unwrap(),
        ledger_before
    );
    fixture.assert_complete_chain();
}

#[test]
fn split_pass_blocks_real_commit_and_seal_is_the_recovery_compatible_successor() {
    let fixture = SealFixture::new("split-verdict");
    let verdict = fixture.orch(&[
        "verdict",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
        "--expected-main",
        &fixture.main_head,
        "--verdict",
        "pass",
    ]);
    assert!(
        verdict.status.success(),
        "verdict stdout={} stderr={}",
        String::from_utf8_lossy(&verdict.stdout),
        String::from_utf8_lossy(&verdict.stderr)
    );
    assert!(String::from_utf8_lossy(&verdict.stderr).contains("屏障"));

    fs::write(fixture.root.join("README.md"), "ordinary commit\n").unwrap();
    git(&fixture.root, &["add", "README.md"]);
    let blocked = git_output(
        &fixture.root,
        &["commit", "--quiet", "-m", "must be blocked after PASS"],
    );
    assert!(
        !blocked.status.success(),
        "ordinary main commit must be blocked"
    );
    let blocked_stderr = String::from_utf8_lossy(&blocked.stderr);
    for needle in [TASK, ATTEMPT, "pending root PASS"] {
        assert!(
            blocked_stderr.contains(needle),
            "hook rejection lacks {needle}: {blocked_stderr}"
        );
    }
    assert_eq!(
        git(&fixture.root, &["rev-parse", "main"]),
        fixture.main_head
    );

    // Restore the staged path without reset/checkout/stash. The durable ledger
    // modification is intentionally retained for seal to consume.
    fs::write(fixture.root.join("README.md"), "base\n").unwrap();
    git(&fixture.root, &["add", "README.md"]);
    let sealed = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        sealed.status.success(),
        "seal after split verdict stdout={} stderr={}",
        String::from_utf8_lossy(&sealed.stdout),
        String::from_utf8_lossy(&sealed.stderr)
    );
    assert!(String::from_utf8_lossy(&sealed.stdout).contains("complete"));
    fixture.assert_complete_chain();
}

#[test]
fn late_review_between_split_verdict_and_seal_is_strictly_audited_but_not_merged() {
    let fixture = SealFixture::new("late-before-seal");
    let verdict = fixture.orch(&[
        "verdict",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
        "--expected-main",
        &fixture.main_head,
        "--verdict",
        "pass",
    ]);
    assert!(
        verdict.status.success(),
        "verdict stdout={} stderr={}",
        String::from_utf8_lossy(&verdict.stdout),
        String::from_utf8_lossy(&verdict.stderr)
    );
    let canonical_bytes = fs::read(fixture.root.join(REVIEW_REL)).unwrap();
    let canonical_binding = fixture.root_review_binding();

    fixture.stage_review_inbox("late review delivered while root PASS barrier is open");
    let delivered = fixture.orch(&[
        "review", "deliver", TASK, ATTEMPT, "--role", "primary", "--agent", REVIEWER,
    ]);
    assert!(
        delivered.status.success(),
        "late deliver stdout={} stderr={}",
        String::from_utf8_lossy(&delivered.stdout),
        String::from_utf8_lossy(&delivered.stderr)
    );
    let late_rel = format!("{}-late-1.md", REVIEW_REL.trim_end_matches(".md"));
    assert!(fixture.root.join(&late_rel).is_file());
    let late_bytes = fs::read(fixture.root.join(&late_rel)).unwrap();
    assert_eq!(
        fs::read(fixture.root.join(REVIEW_REL)).unwrap(),
        canonical_bytes
    );
    assert_eq!(fixture.root_review_binding(), canonical_binding);

    fs::write(fixture.root.join(&late_rel), b"tampered late bytes\n").unwrap();
    let tampered = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        !tampered.status.success(),
        "seal must reject a late artifact whose durable hash no longer matches"
    );
    assert!(!fixture
        .events()
        .iter()
        .any(|event| event.kind == "MergeStarted"));
    fs::write(fixture.root.join(&late_rel), &late_bytes).unwrap();

    let unrelated = fixture.root.join("unrelated-untracked.txt");
    fs::write(&unrelated, b"not a durable late review\n").unwrap();
    let extra_dirty = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        !extra_dirty.status.success(),
        "the exact late allow-list must not authorize another untracked path"
    );
    fs::remove_file(unrelated).unwrap();

    let sealed = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        sealed.status.success(),
        "seal after late delivery stdout={} stderr={}",
        String::from_utf8_lossy(&sealed.stdout),
        String::from_utf8_lossy(&sealed.stderr)
    );
    fixture.assert_complete_chain();
    assert!(
        git(
            &fixture.root,
            &["ls-tree", "-r", "--name-only", "main", "--", &late_rel]
        )
        .is_empty(),
        "late audit artifact must remain outside the authorized merge tree"
    );
    assert_eq!(
        fs::read(fixture.root.join(REVIEW_REL)).unwrap(),
        canonical_bytes
    );
    assert_eq!(fixture.root_review_binding(), canonical_binding);
}

#[test]
fn doctor_fails_with_bound_and_current_hashes_after_recorded_review_is_tampered() {
    let fixture = SealFixture::new("doctor-review-tamper");
    let sealed = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        sealed.status.success(),
        "seal stdout={} stderr={}",
        String::from_utf8_lossy(&sealed.stdout),
        String::from_utf8_lossy(&sealed.stderr)
    );
    fixture.assert_complete_chain();

    fs::write(
        fixture.root.join(REVIEW_REL),
        b"tampered canonical review after TaskRecorded\n",
    )
    .unwrap();
    git(
        &fixture.root,
        &["add", "coordination/rounds/r904/events.jsonl", REVIEW_REL],
    );
    git(
        &fixture.root,
        &[
            "commit",
            "--quiet",
            "-m",
            "commit sealed accounting plus tampered review",
        ],
    );

    let audit = orch_host::verify::audit_recorded_review_bindings(&fixture.root).unwrap();
    let [finding] = audit.findings.as_slice() else {
        panic!("tampered modern recorded review must yield one finding");
    };
    assert_eq!(
        finding.level,
        orch_host::verify::ReviewBindingAuditLevel::Fail
    );
    assert_eq!(finding.path, REVIEW_REL);
    assert_ne!(finding.bound_sha256, finding.current_sha256);

    let doctor = fixture.orch(&["doctor"]);
    assert!(
        !doctor.status.success(),
        "tampered recorded binding must make doctor nonzero"
    );
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    for expected in [
        "level=FAIL",
        REVIEW_REL,
        &format!("boundSha256={}", finding.bound_sha256),
        &format!("currentSha256={}", finding.current_sha256),
    ] {
        assert!(
            stdout.contains(expected),
            "doctor lacks {expected}: {stdout}"
        );
    }
}

#[test]
fn late_review_cli_delivers_two_monotonic_pairs_without_changing_canonical_binding() {
    let fixture = SealFixture::new("late-reviews");
    let sealed = fixture.orch(&[
        "seal",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
    ]);
    assert!(
        sealed.status.success(),
        "seal stdout={} stderr={}",
        String::from_utf8_lossy(&sealed.stdout),
        String::from_utf8_lossy(&sealed.stderr)
    );
    let canonical_bytes = fs::read(fixture.root.join(REVIEW_REL)).unwrap();
    let canonical_binding = fixture.root_review_binding();

    let deliver = |fixture: &SealFixture| {
        fixture.orch(&[
            "review", "deliver", TASK, ATTEMPT, "--role", "primary", "--agent", REVIEWER,
        ])
    };

    fixture.stage_review_inbox("first distinct post-verdict review revision");
    let first = deliver(&fixture);
    assert!(
        first.status.success(),
        "first delivery stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stdout = String::from_utf8_lossy(&first.stdout);
    assert!(first_stdout.contains("-late-1.md"), "{first_stdout}");
    assert!(first_stdout.contains("appendedEvents=2"), "{first_stdout}");
    assert_eq!(
        fs::read(fixture.root.join(REVIEW_REL)).unwrap(),
        canonical_bytes
    );
    assert_eq!(fixture.root_review_binding(), canonical_binding);

    fixture.stage_review_inbox("second distinct post-verdict review revision");
    let second = deliver(&fixture);
    assert!(
        second.status.success(),
        "second delivery stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let second_stdout = String::from_utf8_lossy(&second.stdout);
    assert!(second_stdout.contains("-late-2.md"), "{second_stdout}");
    assert!(
        second_stdout.contains("appendedEvents=2"),
        "{second_stdout}"
    );
    assert_eq!(
        fs::read(fixture.root.join(REVIEW_REL)).unwrap(),
        canonical_bytes
    );
    assert_eq!(fixture.root_review_binding(), canonical_binding);

    let replay = deliver(&fixture);
    assert!(
        replay.status.success(),
        "late delivery replay stdout={} stderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay_stdout = String::from_utf8_lossy(&replay.stdout);
    assert!(replay_stdout.contains("-late-2.md"), "{replay_stdout}");
    assert!(
        replay_stdout.contains("appendedEvents=0"),
        "{replay_stdout}"
    );

    let events = fixture.events();
    for ordinal in [1, 2] {
        let rel = format!("{}-late-{ordinal}.md", REVIEW_REL.trim_end_matches(".md"));
        let matching = events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("path"))
                    .and_then(serde_json::Value::as_str)
                    == Some(rel.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 2, "{rel} must have one durable pair");
        assert_eq!(matching[0].1.kind, "ReviewDelivered");
        assert_eq!(matching[1].1.kind, "EscalationRaised");
        assert_eq!(matching[1].0, matching[0].0 + 1);
        assert_eq!(
            matching[1]
                .1
                .payload
                .as_ref()
                .and_then(|payload| payload.get("stage"))
                .and_then(serde_json::Value::as_str),
            Some("late-review-delivery")
        );
    }
    assert_eq!(
        fs::read(fixture.root.join(REVIEW_REL)).unwrap(),
        canonical_bytes
    );
    assert_eq!(fixture.root_review_binding(), canonical_binding);
}

#[test]
fn approved_reattempt_cli_terminates_a0001_and_replays_a0002_without_minting_a0003() {
    const REASON: &str = "planner approved a fresh implementation before merge";

    let fixture = SealFixture::new("approved-reattempt");
    fixture.write_initial_go();
    let verdict = fixture.orch(&[
        "verdict",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
        "--expected-main",
        &fixture.main_head,
        "--verdict",
        "pass",
    ]);
    assert!(
        verdict.status.success(),
        "verdict stdout={} stderr={}",
        String::from_utf8_lossy(&verdict.stdout),
        String::from_utf8_lossy(&verdict.stderr)
    );
    let verdict_event_id = fixture
        .events()
        .iter()
        .filter(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(TASK)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(ATTEMPT)
        })
        .map(|event| event.event_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(verdict_event_id.len(), 1);
    let verdict_event_id = &verdict_event_id[0];

    let reattempt = fixture.orch(&[
        "dispatch",
        TASK,
        "--new-attempt",
        "--reason",
        REASON,
        "--no-wake",
    ]);
    assert!(
        reattempt.status.success(),
        "reattempt stdout={} stderr={}",
        String::from_utf8_lossy(&reattempt.stdout),
        String::from_utf8_lossy(&reattempt.stderr)
    );
    let stdout = String::from_utf8_lossy(&reattempt.stdout);
    assert!(stdout.contains("B904-A0001 -> B904-A0002"), "{stdout}");

    let assert_successor_chain = || {
        let events = fixture.events();
        let blocked = events
            .iter()
            .filter(|event| {
                event.kind == "AttemptBlocked"
                    && event.actor == "runtime:orch"
                    && event.task_id.as_deref() == Some(TASK)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("approved-reattempt")
            })
            .collect::<Vec<_>>();
        assert_eq!(blocked.len(), 1);
        let payload = blocked[0]
            .payload
            .as_ref()
            .and_then(serde_json::Value::as_object)
            .unwrap();
        assert_eq!(payload.len(), 6, "approved terminal payload must be exact");
        assert_eq!(payload["attemptId"], ATTEMPT);
        assert_eq!(payload["attemptNo"], 1);
        assert_eq!(payload["agent"], "executor-claw");
        assert_eq!(payload["reason"], REASON);
        assert_eq!(payload["verdictEventId"], verdict_event_id.as_str());

        let successor = events
            .iter()
            .filter(|event| {
                event.kind == "DispatchIssued"
                    && event.task_id.as_deref() == Some(TASK)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("attemptId"))
                        .and_then(serde_json::Value::as_str)
                        == Some("B904-A0002")
            })
            .collect::<Vec<_>>();
        assert_eq!(successor.len(), 1, "A0002 dispatch must be exactly once");
        let payload = successor[0].payload.as_ref().unwrap();
        assert_eq!(payload["attemptNo"], 2);
        assert_eq!(payload["baseSha"], fixture.first_dispatch_base);
        assert_eq!(payload["previousAttemptId"], ATTEMPT);
        assert_eq!(payload["previousAgent"], "executor-claw");
        assert_eq!(payload["reassignment"], false);
        let blocked_position = events
            .iter()
            .position(|event| event.event_id == blocked[0].event_id)
            .unwrap();
        let successor_position = events
            .iter()
            .position(|event| event.event_id == successor[0].event_id)
            .unwrap();
        assert_eq!(
            successor_position,
            blocked_position + 1,
            "approved terminal and successor dispatch must be one adjacent atomic batch"
        );
        assert!(!events.iter().any(|event| {
            event.kind == "DispatchIssued"
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some("B904-A0003")
        }));
    };
    assert_successor_chain();

    let replay = fixture.orch(&[
        "dispatch",
        TASK,
        "--new-attempt",
        "--reason",
        REASON,
        "--no-wake",
    ]);
    assert!(
        replay.status.success(),
        "reattempt replay stdout={} stderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    assert!(String::from_utf8_lossy(&replay.stdout).contains("B904-A0001 -> B904-A0002"));
    assert_successor_chain();
}

#[test]
fn approved_reattempt_crash_terminal_blocks_plain_dispatch_and_explicitly_recovers() {
    const REASON: &str = "recover the exact approved transition";

    let fixture = SealFixture::new("approved-terminal-recovery");
    fixture.write_initial_go();
    let verdict = fixture.orch(&[
        "verdict",
        TASK,
        "--attempt",
        ATTEMPT,
        "--expected-head",
        &fixture.task_head,
        "--expected-main",
        &fixture.main_head,
        "--verdict",
        "pass",
    ]);
    assert!(
        verdict.status.success(),
        "verdict stdout={} stderr={}",
        String::from_utf8_lossy(&verdict.stdout),
        String::from_utf8_lossy(&verdict.stderr)
    );
    let verdict_event_id = fixture
        .events()
        .into_iter()
        .find(|event| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.task_id.as_deref() == Some(TASK)
        })
        .unwrap()
        .event_id;
    orch_host::ledger::append(
        &fixture.root,
        ROUND,
        &[orch_host::ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": ATTEMPT,
                "attemptNo": 1,
                "agent": "executor-claw",
                "stage": "approved-reattempt",
                "verdictEventId": verdict_event_id,
                "reason": REASON,
            }),
        )],
    )
    .unwrap();

    let plain = fixture.orch(&["dispatch", TASK, "--no-wake"]);
    assert!(!plain.status.success());
    assert!(
        String::from_utf8_lossy(&plain.stderr).contains("explicit --new-attempt"),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(!fixture.events().iter().any(|event| {
        event.kind == "DispatchIssued"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("attemptId"))
                .and_then(serde_json::Value::as_str)
                == Some("B904-A0002")
    }));

    let recover = fixture.orch(&[
        "dispatch",
        TASK,
        "--new-attempt",
        "--reason",
        REASON,
        "--no-wake",
    ]);
    assert!(
        recover.status.success(),
        "recover stdout={} stderr={}",
        String::from_utf8_lossy(&recover.stdout),
        String::from_utf8_lossy(&recover.stderr)
    );
    assert_eq!(
        fixture
            .events()
            .iter()
            .filter(|event| {
                event.kind == "DispatchIssued"
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("attemptId"))
                        .and_then(serde_json::Value::as_str)
                        == Some("B904-A0002")
            })
            .count(),
        1
    );
}
