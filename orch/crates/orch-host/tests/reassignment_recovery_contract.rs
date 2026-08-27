//! B307 · 重派闭环 immutable contract
//!
//! 首红：compile，缺少 `tierf::REASSIGNMENT_RECOVERY_CONTRACT_V1`，预期 `error[E0432]`。
//!
//! M1 恢复 raw index 字节比较 → `index_format_churn...` 红。
//! M2 跳过逻辑 tree 栅栏 → `content_change...` 红。
//! M3 completed generation 不比 current actionId → `same_attempt_second...` 红。
//! M4 不按最后 DispatchIssued 切段 → `a_new_dispatch...` 红。
//! M5 successor 不写 harnessId → `resume_successor...` 红。
//! M6 successor 不写 terminalCapability → `resume_successor...` 红。
//! M7 reconcile 重复 append → support/internal exact-wake test 红。
//! M8 把终态作用域扩大到 attempt/round → 注入第二 wake 后 support test 红。
//!
//! 本文件永久冻结。不得把测试改弱、改名或追加用例；补充测试放卡面 writeSet 内的
//! `attempt/tests_body.rs`、`tierf/tests/` 或 `wake.rs #[cfg(test)]`。

#![allow(dead_code)]

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_core::EventRecord;
use orch_host::attempt::{self, AttemptRef};
use orch_host::tierf::{self, REASSIGNMENT_RECOVERY_CONTRACT_V1};
use orch_host::{ledger, plan, round};
use sha2::{Digest, Sha256};

const _: u32 = REASSIGNMENT_RECOVERY_CONTRACT_V1;
const ROUND: &str = "r80-fixture";
const TASK: &str = "B307";
const AGENT: &str = "executor-desktop";

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git 启动失败");
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git_status(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("git 启动失败")
        .success()
}

fn sha256(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}

struct SnapshotSite {
    root: PathBuf,
    worktree: PathBuf,
}

impl Drop for SnapshotSite {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn snapshot_site(tag: &str) -> SnapshotSite {
    let root = orch_host::util::test_scratch_dir(&format!("b307-snapshot-{tag}"));
    git(&root, &["init", "-q"]);
    fs::write(root.join("tracked.txt"), "base\n").unwrap();
    fs::write(root.join(".gitignore"), ".worktrees/\n").unwrap();
    git(&root, &["add", "tracked.txt", ".gitignore"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git(&root, &["branch", "-M", "main"]);
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    let wt = root.join(".worktrees/B307");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B307",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(wt.join("tracked.txt"), "unstaged\n").unwrap();
    fs::write(wt.join("staged.txt"), "staged\n").unwrap();
    git(&wt, &["add", "staged.txt"]);
    fs::write(wt.join("untracked.txt"), "untracked\n").unwrap();
    SnapshotSite { root, worktree: wt }
}

#[test]
fn index_format_churn_keeps_a_logically_identical_dirty_snapshot_valid() {
    let site = snapshot_site("index-churn");
    git(&site.worktree, &["update-index", "--index-version=2"]);
    let index = PathBuf::from(git(&site.worktree, &["rev-parse", "--git-path", "index"]));
    let before_sha = sha256(&index);
    let before_tree = git(&site.worktree, &["write-tree"]);
    let before_status = git(&site.worktree, &["status", "--porcelain=v2"]);
    let observed = RefCell::new(None);
    let attempt = AttemptRef {
        task_id: TASK.into(),
        ordinal: 1,
        attempt_id: "B307-A0001".into(),
    };
    let snapshot = attempt::snapshot_worktree_wip_with_hook(
        &site.root,
        ROUND,
        &attempt,
        &mut |phase, _root, worktree| {
            if phase == "between-captures" {
                git(worktree, &["update-index", "--index-version=4"]);
                let after_sha = sha256(&index);
                let after_tree = git(worktree, &["write-tree"]);
                let after_status = git(worktree, &["status", "--porcelain=v2"]);
                *observed.borrow_mut() = Some((after_sha, after_tree, after_status));
            }
            Ok(())
        },
    )
    .expect("纯 index 编码变化不得阻塞逻辑等价的 WIP 快照")
    .expect("dirty worktree 必须产生 snapshot");
    let (after_sha, after_tree, after_status) = observed.into_inner().unwrap();
    assert_ne!(before_sha, after_sha, "测试必须真的改变 index 字节");
    assert_eq!(before_tree, after_tree, "stage tree 必须逻辑等价");
    assert_eq!(before_status, after_status, "porcelain 必须逻辑等价");
    assert!(snapshot.dirty);
    assert!(git_status(
        &site.root,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/{}", snapshot.archive_ref)
        ]
    ));
}

#[test]
fn content_change_still_refuses_and_publishes_no_archive() {
    let site = snapshot_site("content-change");
    let attempt = AttemptRef {
        task_id: TASK.into(),
        ordinal: 1,
        attempt_id: "B307-A0001".into(),
    };
    let result = attempt::snapshot_worktree_wip_with_hook(
        &site.root,
        ROUND,
        &attempt,
        &mut |phase, _root, worktree| {
            if phase == "between-captures" {
                fs::write(worktree.join("tracked.txt"), "changed again\n")?;
            }
            Ok(())
        },
    );
    assert!(result.is_err(), "逻辑内容变化仍必须 fail-closed");
    assert!(!git_status(
        &site.root,
        &[
            "rev-parse",
            "--verify",
            "refs/archive/r80-fixture-B307-A0001-wip",
        ]
    ));
}

struct ResumeSite {
    root: PathBuf,
    worktree: PathBuf,
}

impl Drop for ResumeSite {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn resume_site(tag: &str) -> ResumeSite {
    let root = orch_host::util::test_scratch_dir(&format!("b307-resume-{tag}"));
    git(&root, &["init", "-q"]);
    fs::write(root.join("README.md"), "base\n").unwrap();
    fs::write(
        root.join(".gitignore"),
        "coordination/rounds/*/dispatch/\ncoordination/runtime/\n.worktrees/\n",
    )
    .unwrap();
    git(&root, &["add", "README.md", ".gitignore"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    git(&root, &["branch", "-M", "main"]);
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/modes")).unwrap();
    fs::create_dir_all(root.join(format!("coordination/rounds/{ROUND}/tasks"))).unwrap();
    fs::create_dir_all(root.join(".worktrees")).unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\n\
         git: {pushPolicy: forbidden}\n\
         commands:\n  testGate: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        "preset: relay\n\
         agents:\n  planner: {adapter: root, tier: none}\n  executor: {adapter: codex-desktop, tier: F, agentId: executor-desktop}\n  verifier: {adapter: root-manual, tier: none}\n\
         hitl: {planSignoff: required, mergeGate: auto}\n\
         verification: {mode: root-manual-fixed-head}\n\
         liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}\n\
         scheduling:\n  allowedAgents: [executor-desktop, executor-opencode]\n  capacities:\n    executor-desktop: {agent: 1, quota: 3, roles: [implement]}\n    executor-opencode: {agent: 1, quota: 1, roles: [primary-review]}\n\
         budgets: {round: {maxUsd: 12, wallMinutes: 300, maxModelWakes: 20}}\n\
         git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/runtime/CURRENT-ROUND"),
        format!("{ROUND}\n"),
    )
    .unwrap();
    fs::write(
        root.join(format!("coordination/rounds/{ROUND}/tasks/{TASK}.md")),
        format!(
            "---\ntaskId: {TASK}\nround: {ROUND}\nagent: {AGENT}\n\
             writeSet: []\nfrozenPaths: []\ngates: {{fast: [testGate]}}\n\
             budgets: {{wallMinutes: 30}}\nrequiredReviews:\n\
               - {{role: primary, agent: executor-opencode}}\n\
             requiredEvidence: [fixture]\n---\n# fixture\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_string_pretty(&serde_json::json!({
            "agents": {
                AGENT: {
                    "injectable": true,
                    "sessionId": "fixture",
                    "wake": {"argv": ["/bin/echo", "{\"type\":\"turn.started\"}"]}
                },
                "executor-opencode": {
                    "injectable": false,
                    "sessionId": "",
                    "wake": {"argv": []}
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    plan::run_plan(&root).unwrap();
    round::run_sign_off(&root, Some("B307 seed fixture")).unwrap();
    let wt = root.join(".worktrees/B307");
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task/B307",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    ResumeSite { root, worktree: wt }
}

fn read_events(root: &Path) -> Vec<EventRecord> {
    fs::read_to_string(root.join(format!("coordination/rounds/{ROUND}/events.jsonl")))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn append_dispatch(site: &ResumeSite, attempt_id: &str, attempt_no: usize) -> String {
    let base = git(&site.root, &["rev-parse", "main"]);
    let go_rel = format!("coordination/rounds/{ROUND}/dispatch/{AGENT}/GO-{attempt_id}.md");
    let go = site.root.join(&go_rel);
    fs::create_dir_all(go.parent().unwrap()).unwrap();
    fs::write(&go, "# GO\n").unwrap();
    fs::write(format!("{}.ack", go.display()), "# ACK\n").unwrap();
    let event = ledger::event(
        "DispatchIssued",
        "runtime:test",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "agent": AGENT,
            "baseSha": base,
            "goPath": go_rel,
            "attemptId": attempt_id,
            "attemptNo": attempt_no,
            "wakePending": true,
        }),
    );
    let event_id = event.event_id.clone();
    ledger::append(&site.root, ROUND, &[event]).unwrap();
    event_id
}

fn commit_rejected_report(
    site: &ResumeSite,
    attempt_id: &str,
    attempt_no: usize,
    control_epoch: &str,
    tag: &str,
    reason: &str,
) -> String {
    let rel = format!("coordination/rounds/{ROUND}/reports/{TASK}-REPORT.md");
    let path = site.worktree.join(&rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = format!("---\ntaskId: {TASK}\nattemptId: {attempt_id}\n---\n{tag}\n{reason}\n");
    fs::write(&path, bytes.as_bytes()).unwrap();
    git(&site.worktree, &["add", &rel]);
    git(
        &site.worktree,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            tag,
        ],
    );
    let tip = git(&site.worktree, &["rev-parse", "HEAD"]);
    let digest = hex::encode(Sha256::digest(bytes.as_bytes()));
    let evidence_path = path.to_string_lossy().into_owned();
    let collect_action = format!("collect-{tag}");
    let collect = serde_json::json!({
        "actionId": collect_action,
        "attemptId": attempt_id,
        "attemptNo": attempt_no,
        "evidencePath": evidence_path,
        "evidenceSha256": digest,
        "evidenceLen": bytes.len(),
        "controlEpoch": control_epoch,
        "owner": format!("owner-{tag}"),
        "leaseGeneration": format!("generation-{tag}"),
    });
    ledger::append(
        &site.root,
        ROUND,
        &[
            ledger::event(
                "ReportObserved",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                serde_json::json!({
                    "actionId": "report-observed",
                    "attemptId": attempt_id,
                    "attemptNo": attempt_no,
                    "evidencePath": evidence_path,
                    "evidenceSha256": digest,
                    "evidenceLen": bytes.len(),
                    "controlEpoch": control_epoch,
                }),
            ),
            ledger::event(
                "ReportCollectClaimed",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                collect.clone(),
            ),
            ledger::event(
                "ReportCollectExecuting",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                collect.clone(),
            ),
            ledger::event(
                "MechCheckFailed",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                serde_json::json!({"stage": tag, "reason": reason}),
            ),
            ledger::event(
                "ReportCollectReleased",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                collect,
            ),
            ledger::event(
                "ActionRejected",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                serde_json::json!({
                    "actionId": format!("await-report:{ROUND}:{TASK}"),
                    "operation": "await-report",
                    "attemptId": attempt_id,
                    "attemptNo": attempt_no,
                    "exitCode": 4,
                    "alert": true,
                    "reason": reason,
                }),
            ),
        ],
    )
    .unwrap();
    tip
}

#[test]
fn same_attempt_second_rejection_mints_a_fresh_resume_generation() {
    let site = resume_site("second-rejection");
    let epoch = append_dispatch(&site, "B307-A0001", 1);
    let old_reason = "OLD-REASON-ONE";
    let old_tip =
        commit_rejected_report(&site, "B307-A0001", 1, &epoch, "first-failure", old_reason);
    let (_, prompt_one) = tierf::run_resume(&site.root, TASK).unwrap();
    assert!(prompt_one.contains(old_reason));
    assert!(prompt_one.contains(&old_tip[..7]));

    let new_reason = "LATEST-REASON-TWO";
    let new_tip =
        commit_rejected_report(&site, "B307-A0001", 1, &epoch, "second-failure", new_reason);
    let (_, prompt_two) = tierf::run_resume(&site.root, TASK).unwrap();
    assert_ne!(prompt_one, prompt_two);
    assert!(prompt_two.contains(new_reason));
    assert!(prompt_two.contains(&new_tip[..7]));
    assert!(!prompt_two.contains(old_reason));
    let issued: Vec<_> = read_events(&site.root)
        .into_iter()
        .filter(|event| event.kind == "ResumeIssued")
        .collect();
    assert_eq!(
        issued.len(),
        2,
        "同 attempt 的新失败必须铸造第二代 ResumeIssued"
    );
    assert_ne!(
        issued[0].payload.as_ref().unwrap()["actionId"],
        issued[1].payload.as_ref().unwrap()["actionId"]
    );
    assert_ne!(
        issued[0].payload.as_ref().unwrap()["resumeDigest"],
        issued[1].payload.as_ref().unwrap()["resumeDigest"]
    );
}

#[test]
fn a_new_dispatch_starts_a_new_resume_segment() {
    let site = resume_site("dispatch-segment");
    let epoch_one = append_dispatch(&site, "B307-A0001", 1);
    commit_rejected_report(
        &site,
        "B307-A0001",
        1,
        &epoch_one,
        "attempt-one",
        "A0001-ONLY-REASON",
    );
    tierf::run_resume(&site.root, TASK).unwrap();
    ledger::append(
        &site.root,
        ROUND,
        &[ledger::event(
            "AttemptFailed",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": "B307-A0001",
                "attemptNo": 1,
                "agent": AGENT,
                "reason": "fixture handoff",
            }),
        )],
    )
    .unwrap();
    let epoch_two = append_dispatch(&site, "B307-A0002", 2);
    commit_rejected_report(
        &site,
        "B307-A0002",
        2,
        &epoch_two,
        "attempt-two",
        "A0002-LATEST-REASON",
    );
    let (_, prompt) = tierf::run_resume(&site.root, TASK).unwrap();
    assert!(prompt.contains("B307-A0002"));
    assert!(prompt.contains("A0002-LATEST-REASON"));
    assert!(!prompt.contains("A0001-ONLY-REASON"));
    let last = read_events(&site.root)
        .into_iter()
        .rev()
        .find(|event| event.kind == "ResumeIssued")
        .unwrap();
    assert_eq!(last.payload.unwrap()["attemptId"], "B307-A0002");
}

#[test]
fn resume_successor_carries_the_signed_harness_terminal_contract() {
    let source = include_str!("../src/wake.rs");
    let resume = source
        .split_once("fn dispatch_wake_for_continuation_messages_with_resume(")
        .unwrap()
        .1;
    let payload = resume
        .split_once("let mut wake_payload = serde_json::json!")
        .unwrap()
        .1;
    let payload = payload.split_once("ledger::append(").unwrap().0;
    assert!(payload.contains("\"harnessId\""));
    assert!(payload.contains("\"terminalCapability\""));
    assert!(payload.contains("harness_registry_digest"));
}

#[test]
fn successor_terminal_reconcile_is_wake_scoped_and_idempotent() {
    let source = include_str!("../src/wake.rs");
    let reconcile = source
        .split_once("pub fn reconcile_pending_backend_receipts(")
        .unwrap()
        .1;
    assert!(reconcile.contains("payload_string(event, \"wakeId\") == Some(wake_id)"));
    assert!(reconcile.contains("ManagedWakeTerminated"));
    assert!(reconcile.contains("if !already_terminal"));
    assert!(reconcile.contains("terminal_capability_from_wake(wake)?"));
}
