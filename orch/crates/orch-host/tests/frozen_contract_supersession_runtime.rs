use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::card::{Card, CardMeta};

struct GitRepo {
    root: PathBuf,
    main_oid: String,
}

impl GitRepo {
    fn new(tag: &str) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b270-exact-p-{tag}"));
        git(&root, &["init", "-q", "-b", "main"]);
        write(
            &root,
            "coordination/PROJECT-BINDING.yaml",
            b"git:\n  pushPolicy: allowed\n",
        );
        write(&root, "base.txt", b"base\n");
        commit_all(&root, "base");
        let main_oid = git(&root, &["rev-parse", "HEAD"]);
        Self { root, main_oid }
    }

    fn from_project_main(tag: &str) -> Self {
        let root = orch_host::util::test_scratch_dir(&format!("b270-signed-main-{tag}"));
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("CARGO_MANIFEST_DIR must be inside the project worktree");
        let source_root = source_root
            .to_str()
            .expect("project root must be UTF-8 for git fixture");
        git(
            &root,
            &[
                "clone",
                "-q",
                "--shared",
                "--single-branch",
                "--branch",
                "main",
                source_root,
                ".",
            ],
        );
        let main_oid = git(&root, &["rev-parse", "refs/heads/main^{commit}"]);
        Self { root, main_oid }
    }

    fn branch_from_main(&self, branch: &str, files: &[(&str, &[u8])], subject: &str) -> String {
        git(&self.root, &["checkout", "-q", "main"]);
        git(
            &self.root,
            &["checkout", "-q", "-B", branch, &self.main_oid],
        );
        for (path, bytes) in files {
            write(&self.root, path, bytes);
        }
        commit_all(&self.root, subject);
        git(&self.root, &["rev-parse", "HEAD"])
    }

    fn commit_files(&self, files: &[(&str, &[u8])], subject: &str) -> String {
        for (path, bytes) in files {
            write(&self.root, path, bytes);
        }
        commit_all(&self.root, subject);
        git(&self.root, &["rev-parse", "HEAD"])
    }

    fn park_on_main(&self) {
        git(&self.root, &["checkout", "-q", "main"]);
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git must be available for B270 runtime tests");
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

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, bytes).unwrap();
}

fn commit_all(root: &Path, subject: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=B270",
            "-c",
            "user.email=b270@example.invalid",
            "commit",
            "-q",
            "-m",
            subject,
        ],
    );
}

fn card(write_set: &[&str], seed: Option<(&str, &str)>) -> Card {
    let seeds = seed
        .map(|(src, target)| serde_json::json!([{"src": src, "target": target}]))
        .unwrap_or_else(|| serde_json::json!([]));
    let meta: CardMeta = serde_json::from_value(serde_json::json!({
        "taskId": "T1",
        "seeds": seeds,
        "writeSet": write_set,
        "frozenPaths": [],
        "gates": {"fast": []}
    }))
    .unwrap();
    Card {
        meta,
        body: String::new(),
        rel_path: "coordination/rounds/r-test/tasks/T1.md".to_string(),
    }
}

fn active_signed_round(root: &Path) -> String {
    let rounds_root = root.join("coordination/rounds");
    let mut rounds = fs::read_dir(&rounds_root)
        .expect("project-main clone must contain coordination/rounds")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() || !entry.path().join("ROUND-IR.yaml").is_file() {
                return None;
            }
            let round = entry.file_name().to_str()?.to_string();
            let number = round.strip_prefix('r')?.parse::<u64>().ok()?;
            Some((number, round))
        })
        .collect::<Vec<_>>();
    rounds.sort_by_key(|candidate| candidate.0);

    rounds
        .into_iter()
        .rev()
        .find_map(|(_, round)| {
            let ledger = orch_core::read_ledger(
                &root.join(format!("coordination/rounds/{round}/events.jsonl")),
            )
            .ok()?;
            if !ledger.bad_lines.is_empty() {
                return None;
            }
            orch_host::plan::require_active_round_ir(root, &round, &ledger.events)
                .ok()
                .map(|_| round)
        })
        .expect("project-main clone must contain an active signed round")
}

fn forged_unsigned_supersession_card(root: &Path) -> Card {
    let round = active_signed_round(root);
    let ir = orch_host::plan::load_round_ir(root, &round)
        .expect("active signed ROUND-IR must load from the isolated clone");
    let task = ir
        .tasks
        .iter()
        .find(|task| {
            task.required_reviews.len() == 2
                && task
                    .required_reviews
                    .iter()
                    .any(|review| review.role == "primary")
                && task
                    .required_reviews
                    .iter()
                    .any(|review| review.role == "secondary")
        })
        .expect("active signed ROUND-IR must contain a dual-review task");
    let exact = orch_host::card::load(root, &round, &task.id)
        .expect("exact signed task card must load from the isolated clone");
    assert_eq!(
        exact.meta.required_reviews, task.required_reviews,
        "exact card and signed ROUND-IR must bind the same dual-review seats"
    );
    assert_eq!(exact.meta.round.as_deref(), Some(round.as_str()));
    assert!(
        ir.source_bindings.task_cards.contains_key(&exact.rel_path),
        "exact card path must be present in signed sourceBindings"
    );

    let hash = |character: char| character.to_string().repeat(64);
    let mut meta = exact.meta.clone();
    let task_id = meta.task_id.clone();
    let target = meta
        .write_set
        .first()
        .expect("exact signed task card must have a writeSet")
        .clone();
    let reviews = meta.required_reviews.clone();
    let declaration = serde_json::from_value(serde_json::json!({
        "target": target,
        "initiator": task_id,
        "originalSeedRelocated": {
            "eventId": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            "sha256": hash('a')
        },
        "effectiveAnchor": {
            "kind": "seed-relocated",
            "eventId": "01BX5ZZKBKACTAV9WEVGEMMVRZ",
            "sha256": hash('a')
        },
        "oldFileSha256": hash('a'),
        "newFileSha256": hash('b'),
        "oldLiteralSha256": hash('c'),
        "newLiteralSha256": hash('d'),
        "subjectPrefix": {
            "path": "orch/crates/orch-host/tests/frozen_contract_supersession_runtime.rs",
            "bytes": 4096
        },
        "blockedAttempt": {
            "round": "r70",
            "taskId": "B253",
            "attemptId": "B253-A0001",
            "eventId": "01CX5ZZKBKACTAV9WEVGEMMVRZ"
        },
        "replacement": {
            "round": "r71",
            "taskId": "B269",
            "taskRecordedEventId": "01DX5ZZKBKACTAV9WEVGEMMVRZ"
        },
        "dispositions": [{
            "oldAssertion": "old assertion",
            "replacementAssertion": "replacement assertion"
        }],
        "reviews": reviews
    }))
    .expect("forged supersession fixture must be internally valid JSON");
    meta.frozen_contract_supersessions = vec![declaration];
    orch_host::card::validate_frozen_contract_supersessions(&meta).unwrap();
    Card {
        meta,
        body: exact.body,
        rel_path: exact.rel_path,
    }
}

fn check(root: &Path, candidate_oid: &str, card: &Card) -> anyhow::Result<()> {
    orch_host::mech::check(
        root,
        card,
        candidate_oid,
        "coordination/rounds/r-test/reports/T1-REPORT.md",
        None,
    )
    .map(|_| ())
}

fn landed_probe_target(root: &Path) -> String {
    let bytes = fs::read(root.join("coordination/frozen-contract-baseline-v1.json"))
        .expect("signed landed baseline must exist in project main");
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).expect("signed landed baseline must be JSON");
    let target = manifest
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .expect("signed landed baseline must contain targets")
        .iter()
        .find_map(|entry| {
            let path = entry.get("target")?.as_str()?;
            let present = entry.get("state")?.as_str()? == "present";
            let relocated =
                entry.pointer("/effectiveAnchor/kind")?.as_str()? == "seed-relocated";
            (present && relocated && path.starts_with("orch/crates/orch-host/tests/"))
                .then(|| path.to_string())
        })
        .expect("baseline must contain a present orch-host SeedRelocated target");
    assert!(
        root.join(&target).is_file(),
        "selected landed target must exist: {target}"
    );
    target
}

fn tampered_landed_bytes(root: &Path, target: &str) -> Vec<u8> {
    let mut bytes = fs::read(root.join(target)).expect("landed probe target must be readable");
    bytes.extend_from_slice(b"\n// B270 exact-P landed-guard mutation\n");
    bytes
}

#[test]
fn exact_candidate_diff_rejects_bad_p_while_task_ref_points_to_clean_q() {
    let repo = GitRepo::new("bad-p-clean-q-diff");
    let bad_p = repo.branch_from_main(
        "candidate-p",
        &[("outside-write-set.txt", b"smuggled\n")],
        "bad P",
    );
    let _clean_q = repo.branch_from_main("task/T1", &[("allowed.txt", b"clean Q\n")], "clean Q");
    repo.park_on_main();

    let error = check(&repo.root, &bad_p, &card(&["allowed.txt"], None))
        .expect_err("diff must inspect exact bad P, not movable clean Q");
    assert!(error.to_string().contains("文件域越界"), "{error:#}");
}

#[test]
fn exact_candidate_diff_accepts_clean_p_while_task_ref_points_to_bad_q_or_is_deleted() {
    let repo = GitRepo::new("clean-p-bad-q-diff");
    let clean_p = repo.branch_from_main("candidate-p", &[("allowed.txt", b"clean P\n")], "clean P");
    let _bad_q =
        repo.branch_from_main("task/T1", &[("outside-write-set.txt", b"bad Q\n")], "bad Q");
    repo.park_on_main();
    let task = card(&["allowed.txt"], None);

    check(&repo.root, &clean_p, &task).expect("movable bad Q must not contaminate exact clean P");
    git(&repo.root, &["update-ref", "-d", "refs/heads/task/T1"]);
    check(&repo.root, &clean_p, &task)
        .expect("deleting the task ref must not invalidate captured clean P");
}

#[test]
fn exact_candidate_commit_walk_uses_p_not_the_clean_task_ref() {
    let repo = GitRepo::new("bad-p-clean-q-commit");
    write(&repo.root, "seed/contract.rs", b"contract\n");
    commit_all(&repo.root, "add bound seed source");
    let main_oid = git(&repo.root, &["rev-parse", "HEAD"]);
    let repo = GitRepo {
        root: repo.root,
        main_oid,
    };

    let bad_p = repo.branch_from_main(
        "candidate-p",
        &[
            ("tests/contract.rs", b"contract\n"),
            ("allowed.txt", b"smuggled into seed commit\n"),
        ],
        "bad seed shape P",
    );
    let _clean_q = repo.branch_from_main(
        "task/T1",
        &[("tests/contract.rs", b"contract\n")],
        "clean seed Q",
    );
    repo.park_on_main();

    let task = card(
        &["tests/contract.rs", "allowed.txt"],
        Some(("seed/contract.rs", "tests/contract.rs")),
    );
    let error = check(&repo.root, &bad_p, &task)
        .expect_err("first-commit shape must be derived from exact bad P");
    assert!(
        error.to_string().contains("首 commit 应只含种子搬运"),
        "{error:#}"
    );
}

#[test]
fn exact_candidate_seed_blob_uses_p_not_the_clean_task_ref() {
    let repo = GitRepo::new("bad-p-clean-q-blob");
    write(&repo.root, "seed/contract.rs", b"contract\n");
    commit_all(&repo.root, "add bound seed source");
    let main_oid = git(&repo.root, &["rev-parse", "HEAD"]);
    let repo = GitRepo {
        root: repo.root,
        main_oid,
    };

    repo.branch_from_main(
        "candidate-p",
        &[("tests/contract.rs", b"contract\n")],
        "seed P",
    );
    let bad_p = repo.commit_files(
        &[("tests/contract.rs", b"tampered after relocation\n")],
        "tamper P",
    );
    let _clean_q = repo.branch_from_main(
        "task/T1",
        &[("tests/contract.rs", b"contract\n")],
        "clean seed Q",
    );
    repo.park_on_main();

    let task = card(
        &["tests/contract.rs"],
        Some(("seed/contract.rs", "tests/contract.rs")),
    );
    let error = check(&repo.root, &bad_p, &task)
        .expect_err("final seed blob must be read from exact tampered P");
    assert!(error.to_string().contains("种子字节不一致"), "{error:#}");
}

#[test]
fn exact_candidate_landed_guard_rejects_bad_p_while_refs_are_clean() {
    let repo = GitRepo::from_project_main("bad-p-clean-refs");
    let target = landed_probe_target(&repo.root);
    let tampered = tampered_landed_bytes(&repo.root, &target);
    let bad_p = repo.branch_from_main(
        "candidate-p",
        &[(target.as_str(), tampered.as_slice())],
        "tamper landed target in P",
    );
    let _clean_q = repo.branch_from_main(
        "task/T1",
        &[("clean-q.txt", b"clean movable Q\n")],
        "clean movable Q",
    );
    repo.park_on_main();

    let task = card(&[target.as_str()], None);
    let error = check(&repo.root, &bad_p, &task)
        .expect_err("exact bad P must be rejected even when HEAD/task ref are clean");
    let message = format!("{error:#}");
    assert!(
        message.contains("无签名 supersession") && message.contains(&target),
        "{message}"
    );
}

#[test]
fn exact_candidate_landed_guard_accepts_clean_p_while_refs_are_bad() {
    let repo = GitRepo::from_project_main("clean-p-bad-refs");
    let target = landed_probe_target(&repo.root);
    let tampered = tampered_landed_bytes(&repo.root, &target);
    let clean_p = repo.branch_from_main(
        "candidate-p",
        &[("allowed.txt", b"clean exact P\n")],
        "clean exact P",
    );
    let _bad_q = repo.branch_from_main(
        "task/T1",
        &[(target.as_str(), tampered.as_slice())],
        "tampered movable Q",
    );

    check(&repo.root, &clean_p, &card(&["allowed.txt"], None))
        .expect("bad movable refs must not contaminate exact clean P");
}

#[test]
fn ordinary_append_and_append_checked_cannot_mint_a_frozen_supersession() {
    let repo = GitRepo::new("ordinary-append-refused");
    let forged = orch_host::ledger::event(
        "FrozenContractSuperseded",
        "runtime:orch",
        Some("T1"),
        Some("r-test"),
        serde_json::json!({"forged": true}),
    );

    let error = orch_host::ledger::append(&repo.root, "r-test", &[forged.clone()])
        .expect_err("ordinary append must not possess merge-lifecycle authority");
    assert!(
        error.to_string().contains("exclusive merge transition"),
        "{error:#}"
    );
    let error = orch_host::ledger::append_checked(&repo.root, "r-test", |_| {
        Ok(vec![forged.clone()])
    })
    .expect_err("ordinary append_checked must not possess merge-lifecycle authority");
    assert!(
        error.to_string().contains("exclusive merge transition"),
        "{error:#}"
    );

    let ledger = repo
        .root
        .join("coordination/rounds/r-test/events.jsonl");
    assert!(
        !ledger.exists() || fs::read(&ledger).unwrap().is_empty(),
        "rejected ordinary append must leave no durable event"
    );
}

#[test]
fn landed_baseline_descriptor_must_be_covered_by_the_signed_binding_bytes() {
    let repo = GitRepo::from_project_main("binding-drift");
    let binding_path = "coordination/PROJECT-BINDING.yaml";
    let mut binding = fs::read(repo.root.join(binding_path)).unwrap();
    binding.extend_from_slice(b"\n# unsigned B270 binding drift\n");
    write(&repo.root, binding_path, &binding);
    commit_all(&repo.root, "unsigned binding drift");
    let drifted_main = git(&repo.root, &["rev-parse", "HEAD"]);
    git(
        &repo.root,
        &["update-ref", "refs/heads/main", &drifted_main],
    );
    let repo = GitRepo {
        root: repo.root,
        main_oid: drifted_main,
    };
    let candidate = repo.branch_from_main(
        "candidate-p",
        &[("allowed.txt", b"candidate\n")],
        "candidate",
    );
    repo.park_on_main();

    let error = check(&repo.root, &candidate, &card(&["allowed.txt"], None))
        .expect_err("unsigned binding bytes must not authorize the baseline descriptor");
    assert!(
        format!("{error:#}").contains("PROJECT-BINDING bytes 未绑定 signed ROUND-IR"),
        "{error:#}"
    );
}

#[test]
fn a_caller_forged_supersession_is_rejected_against_the_exact_signed_card() {
    let repo = GitRepo::from_project_main("forged-card");
    let forged = forged_unsigned_supersession_card(&repo.root);
    let error = check(&repo.root, &repo.main_oid, &forged)
        .expect_err("caller-provided supersession must not replace exact signed card bytes");
    assert!(
        format!("{error:#}")
            .contains("caller supersession declaration 与 exact signed card 不一致"),
        "{error:#}"
    );
}
