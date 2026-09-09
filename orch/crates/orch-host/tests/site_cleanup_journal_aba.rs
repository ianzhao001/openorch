//! B314 · complete cleanup journal 后同 generation ABA 的 immutable contract。
//!
//! Expected red: compile `error[E0432]`。无状态 public contract anchor
//! `COMPLETE_JOURNAL_ABA_CONTRACT_V1` 尚不存在；补 anchor 后，现状 `sites.rs::reap_one` 看到
//! `journal.phase == "complete"` 且同一路径/registry 再次出现时，仍直接返回
//! `complete site cleanup journal 与物理/registry 状态冲突`，所以行为测试继续红，不能只补常量凑绿。
//!
//! 允许的恢复非常窄：账本仍须把该 exact `(siteId, generation)` 折叠为 Released，重现现场
//! 必须是 canonical 路径、同 common-dir、detached、同 reviewedHead、clean、无 symlink；满足后
//! 只重做物理删除，不新增任何 ledger event。active、dirty、symlink、HEAD 漂移都必须保留现场。
//!
//! Negative mutations that must turn a named case red:
//! M1. 保留 complete-journal 见物理路径即 bail 的旧分支
//!     -> `complete_journal_replays_exact_released_generation_after_aba` 红。
//! M2. 不再复算 exact Released `(siteId,generation)`，按目录年龄或名字删
//!     -> `active_same_generation_is_never_recleaned` 红；证据歧义可为 Err 或 refusal，
//!        但 worktree/target/ledger 必须原样保留。
//! M3. 忽略 tracked/staged dirty 状态
//!     -> `dirty_recreated_worktree_is_refused_and_preserved` 红。
//! M4. 忽略 detached HEAD / reviewedHead 身份
//!     -> `head_drift_is_refused_and_preserved` 红。
//! M5. 跟随 worktree/target 的 symlink
//!     -> `symlink_recreation_is_refused_without_touching_target` 红。
//! M6. exact replay 或第二次幂等 replay 追加任何事件
//!     -> 第一个测试的 ledger byte equality 红。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use orch_host::ledger;
use orch_host::sites::{
    reap_released_sites_reported, COMPLETE_JOURNAL_ABA_CONTRACT_V1,
    MANAGED_COMPLETION_RECEIPT,
};
use orch_host::util::test_scratch_dir;

const ROUND: &str = "rT";
const TASK: &str = "BT";
const ATTEMPT: &str = "BT-A0001";
const AGENT: &str = "executor-pi";
const SITE_ID: &str = "BT-secondary-executor-pi-g01";
const WORKTREE: &str = ".worktrees/review-BT-A0001-secondary-executor-pi-g01";
const TARGET: &str = "orch/target/review-BT-A0001-secondary-executor-pi-g01";

struct Fixture {
    root: PathBuf,
    reviewed_head: String,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn add_detached_worktree(root: &Path, head: &str) {
    let path = root.join(WORKTREE);
    fs::create_dir_all(path.parent().expect("worktree parent")).expect("create worktree parent");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg(head)
        .output()
        .expect("spawn git worktree add");
    assert!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::create_dir_all(root.join(TARGET)).expect("create target");
    fs::write(root.join(TARGET).join("artifact.bin"), b"reclaim me").expect("write target");
}

fn event(kind: &str, payload: serde_json::Value) -> orch_core::EventRecord {
    ledger::event(kind, "runtime:orch", Some(TASK), Some(ROUND), payload)
}

fn lease_event() -> orch_core::EventRecord {
    event(
        "WorkspaceLeased",
        serde_json::json!({
            "siteId": SITE_ID,
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "secondary",
            "agent": AGENT,
            "reviewedHead": "PLACEHOLDER",
            "wakeId": "wake-b314",
            "paths": {"worktree": WORKTREE, "target": TARGET},
        }),
    )
}

fn released_event() -> orch_core::EventRecord {
    event(
        "WorkspaceReleased",
        serde_json::json!({
            "siteId": SITE_ID,
            "generation": 1,
            "attemptId": ATTEMPT,
            "role": "secondary",
            "agent": AGENT,
            "wakeId": "wake-b314",
            "completionReceipt": MANAGED_COMPLETION_RECEIPT,
        }),
    )
}

fn fixture(tag: &str) -> Fixture {
    let root = test_scratch_dir(&format!("b314-{tag}"));
    git(&root, &["init", "-b", "main"]);
    fs::write(root.join("tracked.txt"), b"baseline\n").expect("write baseline");
    git(&root, &["add", "tracked.txt"]);
    git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "baseline",
        ],
    );
    let reviewed_head = git(&root, &["rev-parse", "HEAD"]);
    fs::create_dir_all(root.join("coordination/rounds").join(ROUND)).expect("create ledger dir");
    fs::create_dir_all(root.join("coordination/runtime/ledger-wal")).expect("create WAL dir");
    fs::write(
        root.join("coordination/rounds")
            .join(ROUND)
            .join("events.jsonl"),
        b"",
    )
    .expect("create ledger");
    fs::write(
        root.join("coordination/runtime/ledger-wal")
            .join(format!("{ROUND}.jsonl")),
        b"",
    )
    .expect("create WAL");
    add_detached_worktree(&root, &reviewed_head);

    let mut lease = lease_event();
    lease.payload.as_mut().expect("lease payload")["reviewedHead"] =
        serde_json::json!(reviewed_head.clone());
    ledger::append(&root, ROUND, &[lease, released_event()]).expect("append released site");
    Fixture {
        root,
        reviewed_head,
    }
}

fn complete_once(site: &Fixture) {
    let first = reap_released_sites_reported(&site.root, ROUND).expect("initial cleanup");
    assert_eq!(first.reaped, vec![SITE_ID.to_string()]);
    assert!(!site.root.join(WORKTREE).exists());
    assert!(!site.root.join(TARGET).exists());
    let journal = fs::read_to_string(
        site.root
            .join("coordination/runtime/site-cleanup")
            .join(ROUND)
            .join(format!("{SITE_ID}.json")),
    )
    .expect("read complete journal");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&journal).expect("parse journal")["phase"],
        "complete"
    );
}

fn ledger_bytes(root: &Path) -> Vec<u8> {
    fs::read(
        root.join("coordination/rounds")
            .join(ROUND)
            .join("events.jsonl"),
    )
    .expect("read ledger")
}

#[test]
fn complete_journal_aba_contract_anchor_is_version_one() {
    assert_eq!(COMPLETE_JOURNAL_ABA_CONTRACT_V1, 1);
}

#[test]
fn complete_journal_replays_exact_released_generation_after_aba() {
    let site = fixture("exact");
    complete_once(&site);
    add_detached_worktree(&site.root, &site.reviewed_head);
    let before = ledger_bytes(&site.root);

    let replay = reap_released_sites_reported(&site.root, ROUND)
        .expect("exact released generation must permit physical replay");
    assert_eq!(replay.reaped, vec![SITE_ID.to_string()]);
    assert!(!site.root.join(WORKTREE).exists());
    assert!(!site.root.join(TARGET).exists());
    assert_eq!(
        ledger_bytes(&site.root),
        before,
        "physical replay emits no event"
    );

    let idempotent = reap_released_sites_reported(&site.root, ROUND).expect("idempotent replay");
    assert!(idempotent.reaped.is_empty());
    assert_eq!(
        ledger_bytes(&site.root),
        before,
        "second replay is byte-idempotent"
    );
}

#[test]
fn active_same_generation_is_never_recleaned() {
    let site = fixture("active");
    complete_once(&site);
    add_detached_worktree(&site.root, &site.reviewed_head);
    let mut duplicate = lease_event();
    duplicate.payload.as_mut().expect("lease payload")["reviewedHead"] =
        serde_json::json!(site.reviewed_head.clone());
    ledger::append(&site.root, ROUND, &[duplicate]).expect("append active duplicate");
    let before = ledger_bytes(&site.root);

    let result = reap_released_sites_reported(&site.root, ROUND);
    if let Ok(report) = result {
        assert!(report.reaped.is_empty());
    }
    assert!(site.root.join(WORKTREE).exists());
    assert!(site.root.join(TARGET).exists());
    assert_eq!(
        ledger_bytes(&site.root),
        before,
        "ambiguous active evidence may be Err or refusal, but must preserve site and ledger"
    );
}

#[test]
fn dirty_recreated_worktree_is_refused_and_preserved() {
    let site = fixture("dirty");
    complete_once(&site);
    add_detached_worktree(&site.root, &site.reviewed_head);
    fs::write(site.root.join(WORKTREE).join("tracked.txt"), b"dirty\n").expect("dirty worktree");

    let report = reap_released_sites_reported(&site.root, ROUND)
        .expect("dirty ABA must be a reported refusal, not deletion");
    assert!(report.reaped.is_empty());
    assert_eq!(report.refused.len(), 1);
    assert!(site.root.join(WORKTREE).exists());
    assert!(site.root.join(TARGET).exists());
}

#[test]
fn head_drift_is_refused_and_preserved() {
    let site = fixture("head-drift");
    complete_once(&site);
    fs::write(site.root.join("tracked.txt"), b"new head\n").expect("write second commit");
    git(&site.root, &["add", "tracked.txt"]);
    git(
        &site.root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-m",
            "new-head",
        ],
    );
    let drifted_head = git(&site.root, &["rev-parse", "HEAD"]);
    assert_ne!(drifted_head, site.reviewed_head);
    add_detached_worktree(&site.root, &drifted_head);

    let report = reap_released_sites_reported(&site.root, ROUND)
        .expect("HEAD drift must be a reported refusal, not deletion");
    assert!(report.reaped.is_empty());
    assert_eq!(report.refused.len(), 1);
    assert!(site.root.join(WORKTREE).exists());
    assert!(site.root.join(TARGET).exists());
}

#[cfg(unix)]
#[test]
fn symlink_recreation_is_refused_without_touching_target() {
    use std::os::unix::fs::symlink;

    let site = fixture("symlink");
    complete_once(&site);
    add_detached_worktree(&site.root, &site.reviewed_head);
    fs::remove_dir_all(site.root.join(TARGET)).expect("remove canonical target");
    let outside = site.root.join("outside-sentinel");
    fs::create_dir_all(&outside).expect("create outside target");
    fs::write(outside.join("keep.txt"), b"keep\n").expect("write sentinel");
    symlink(&outside, site.root.join(TARGET)).expect("create target symlink");

    let result = reap_released_sites_reported(&site.root, ROUND);
    if let Ok(report) = result {
        assert!(report.reaped.is_empty());
        assert_eq!(report.refused.len(), 1);
    }
    assert_eq!(
        fs::read(outside.join("keep.txt")).expect("sentinel"),
        b"keep\n"
    );
    assert!(site.root.join(WORKTREE).exists());
    let target = site.root.join(TARGET);
    assert!(
        fs::symlink_metadata(&target)
            .expect("target symlink must be preserved")
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_link(&target).expect("read preserved symlink"), outside);
}
