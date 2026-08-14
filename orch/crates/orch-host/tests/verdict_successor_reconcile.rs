//! B180 seeded-red contract: committed review reconcile.
//!
//! Expected red: compile. The public reconcile API is intentionally absent before B180.
//!
//! M1: read a review from the mutable working tree instead of the captured main commit.
//! M2: release a different task/attempt/role or an unrequested review artifact.
//! M3: decide outside the ledger lock so concurrent reconciliation duplicates delivery.
//! M4: append from a moving main ref instead of one fixed full commit SHA.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::util::test_scratch_dir;
use orch_host::wake::reconcile_committed_review_delivery_slots;
use serde_json::json;

const ROUND: &str = "r58";
const BODY: &str = "committed review body";
const HEAD: &str = "0123456789012345678901234567890123456789";
const WRONG_HEAD: &str = "1123456789012345678901234567890123456789";

fn temp_root() -> PathBuf {
    // H92：原写法把 scratch 落在机器共享的系统临时目录，按 `pid + 纳秒` 命名。同一测试
    // 二进制内所有线程的 pid 相同，而时钟粒度不保证纳秒唯一，两线程可能算出同一个 root：
    // create_dir_all 幂等地放行，随后 git init 的模板拷贝竞态失败，把整轮全量门打红。
    // 唯一性改由 B108 契约助手承担（repo 本地 target/test-tmp + pid + 单调序号，不读时钟）。
    test_scratch_dir("orch-b180-reconcile")
}

fn git(root: &Path, args: &[&str]) {
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
}

fn review_path(root: &Path, attempt: &str, role: &str, agent: &str) -> PathBuf {
    root.join(format!(
        "coordination/rounds/{ROUND}/reviews/{attempt}-{role}-{agent}.md"
    ))
}

fn review_bytes(task: &str, attempt: &str, role: &str, agent: &str) -> Vec<u8> {
    format!(
        "---\ntaskId: {task}\nround: {ROUND}\nattemptId: {attempt}\nrole: {role}\nreviewer: {agent}\nverdict: PASS\nreviewedHead: {HEAD}\n---\n{BODY}\n"
    )
    .into_bytes()
}

fn collect(task: &str, attempt: &str) -> orch_core::EventRecord {
    orch_host::ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(task),
        Some(ROUND),
        json!({
            "attemptId": attempt,
            "branchSha": HEAD,
        }),
    )
}

fn request(task: &str, attempt: &str, role: &str, agent: &str) -> orch_core::EventRecord {
    orch_host::ledger::event(
        "ReviewRequested",
        "runtime:orch",
        Some(task),
        Some(ROUND),
        json!({
            "attemptId": attempt,
            "role": role,
            "agent": agent,
            "deadlineSecs": 1800,
            "requestedAt": "2026-07-30T00:00:00Z"
        }),
    )
}

fn setup_reconcile_fixture() -> PathBuf {
    let root = temp_root();
    git(&root, &["init", "-q"]);
    git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}/reviews"))).unwrap();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();

    orch_host::ledger::append(
        &root,
        ROUND,
        &[
            collect("B180", "B180-A0002"),
            request("B180", "B180-A0002", "primary", "executor-claw"),
            collect("B180", "B180-A0001"),
            request("B180", "B180-A0001", "secondary", "executor-opencode"),
            collect("B999", "B999-A0001"),
            request("B999", "B999-A0001", "primary", "executor-claw"),
        ],
    )
    .unwrap();

    for (task, attempt, role, agent) in [
        ("B180", "B180-A0002", "primary", "executor-claw"),
        ("B180", "B180-A0001", "secondary", "executor-opencode"),
        ("B999", "B999-A0001", "primary", "executor-claw"),
    ] {
        fs::write(
            review_path(&root, attempt, role, agent),
            review_bytes(task, attempt, role, agent),
        )
        .unwrap();
    }

    git(
        &root,
        &[
            "add",
            "--",
            "coordination/runtime/CURRENT-ROUND",
            "coordination/rounds/r58/events.jsonl",
            "coordination/rounds/r58/reviews",
        ],
    );
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );

    // The exact committed review is blank in the working tree, while this
    // unrequested sibling exists only in the working tree. Neither observation
    // may influence reconciliation from the fixed main commit.
    fs::write(
        review_path(
            &root,
            "B180-A0002",
            "primary",
            "executor-claw",
        ),
        b"---\n---\n",
    )
    .unwrap();
    fs::write(
        review_path(
            &root,
            "B180-A0002",
            "secondary",
            "executor-opencode",
        ),
        review_bytes(
            "B180",
            "B180-A0002",
            "secondary",
            "executor-opencode",
        ),
    )
    .unwrap();
    root
}

fn setup_contract_fixture(review: &[u8], include_collect: bool) -> PathBuf {
    let root = temp_root();
    git(&root, &["init", "-q"]);
    git(&root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}/reviews"))).unwrap();
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    let mut events = Vec::new();
    if include_collect {
        events.push(collect("B180", "B180-A0001"));
    }
    events.push(request("B180", "B180-A0001", "primary", "executor-claw"));
    orch_host::ledger::append(&root, ROUND, &events).unwrap();
    let rel = "coordination/rounds/r58/reviews/B180-A0001-primary-executor-claw.md";
    fs::write(root.join(rel), review).unwrap();
    git(
        &root,
        &[
            "add",
            "--",
            "coordination/runtime/CURRENT-ROUND",
            "coordination/rounds/r58/events.jsonl",
            rel,
        ],
    );
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-qm",
            "contract fixture",
        ],
    );
    root
}

fn delivered_count(root: &Path) -> usize {
    orch_core::read_ledger(&root.join(format!("coordination/rounds/{ROUND}/events.jsonl")))
        .unwrap()
        .events
        .iter()
        .filter(|event| event.kind == "ReviewDelivered")
        .count()
}

#[test]
fn committed_reconcile_is_exact_concurrent_and_idempotent() {
    let root = setup_reconcile_fixture();
    let first_root = root.clone();
    let second_root = root.clone();
    let first = std::thread::spawn(move || {
        reconcile_committed_review_delivery_slots(
            &first_root,
            "B180",
            "B180-A0002",
        )
        .unwrap()
    });
    let second = std::thread::spawn(move || {
        reconcile_committed_review_delivery_slots(
            &second_root,
            "B180",
            "B180-A0002",
        )
        .unwrap()
    });
    assert_eq!(first.join().unwrap() + second.join().unwrap(), 1);
    assert_eq!(
        reconcile_committed_review_delivery_slots(
            &root,
            "B180",
            "B180-A0002",
        )
        .unwrap(),
        0
    );

    let ledger = orch_core::read_ledger(
        &root.join("coordination/rounds/r58/events.jsonl"),
    )
    .unwrap();
    let delivered = ledger
        .events
        .iter()
        .filter(|event| event.kind == "ReviewDelivered")
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1);
    let event = delivered[0];
    assert_eq!(event.actor, "runtime:orch");
    assert_eq!(event.task_id.as_deref(), Some("B180"));
    let payload = event.payload.as_ref().unwrap();
    assert_eq!(
        payload.get("attemptId").and_then(serde_json::Value::as_str),
        Some("B180-A0002")
    );
    assert_eq!(
        payload.get("role").and_then(serde_json::Value::as_str),
        Some("primary")
    );
    assert_eq!(
        payload.get("agent").and_then(serde_json::Value::as_str),
        Some("executor-claw")
    );
    assert_eq!(
        payload.get("bodyLen").and_then(serde_json::Value::as_u64),
        Some(BODY.len() as u64),
        "bodyLen must come from the committed blob, not the blank working copy"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn committed_reconcile_rejects_each_missing_required_field() {
    let complete = String::from_utf8(review_bytes(
        "B180",
        "B180-A0001",
        "primary",
        "executor-claw",
    ))
    .unwrap();
    for (field, line) in [
        ("taskId", "taskId: B180\n"),
        ("round", "round: r58\n"),
        ("attemptId", "attemptId: B180-A0001\n"),
        ("role", "role: primary\n"),
        ("reviewer", "reviewer: executor-claw\n"),
        ("verdict", "verdict: PASS\n"),
        (
            "reviewedHead",
            concat!(
                "reviewedHead: ",
                "0123456789012345678901234567890123456789",
                "\n"
            ),
        ),
    ] {
        let bytes = complete.replacen(line, "", 1).into_bytes();
        let root = setup_contract_fixture(&bytes, true);
        let result = reconcile_committed_review_delivery_slots(&root, "B180", "B180-A0001");
        assert!(result.is_err(), "missing {field} must fail closed");
        assert_eq!(delivered_count(&root), 0, "missing {field} delivered");
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn committed_reconcile_rejects_head_mismatch_and_unrecoverable_legacy_request() {
    let wrong = String::from_utf8(review_bytes(
        "B180",
        "B180-A0001",
        "primary",
        "executor-claw",
    ))
    .unwrap()
    .replace(HEAD, WRONG_HEAD)
    .into_bytes();
    let wrong_root = setup_contract_fixture(&wrong, true);
    assert!(reconcile_committed_review_delivery_slots(&wrong_root, "B180", "B180-A0001").is_err());
    assert_eq!(delivered_count(&wrong_root), 0);
    fs::remove_dir_all(wrong_root).unwrap();

    let complete = review_bytes("B180", "B180-A0001", "primary", "executor-claw");
    let no_collect = setup_contract_fixture(&complete, false);
    assert!(reconcile_committed_review_delivery_slots(&no_collect, "B180", "B180-A0001").is_err());
    assert_eq!(delivered_count(&no_collect), 0);
    fs::remove_dir_all(no_collect).unwrap();

    let no_collect_or_artifact = setup_contract_fixture(&complete, false);
    let rel = "coordination/rounds/r58/reviews/B180-A0001-primary-executor-claw.md";
    git(&no_collect_or_artifact, &["rm", "-q", "--", rel]);
    git(
        &no_collect_or_artifact,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-qm",
            "remove review artifact",
        ],
    );
    assert!(reconcile_committed_review_delivery_slots(
        &no_collect_or_artifact,
        "B180",
        "B180-A0001"
    )
    .is_err());
    assert_eq!(delivered_count(&no_collect_or_artifact), 0);
    fs::remove_dir_all(no_collect_or_artifact).unwrap();
}
