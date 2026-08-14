//! 主仓写保护守卫（r55/B162，H28 决议②）：reference-transaction hook 的策略判定、
//! 幂等安装与违规留证。背景：r51/r53 两次被 `reset --hard` / `checkout <sha>` 抹掉
//! 未提交的 durable 事件；B160 已先给每个 agent 仓内可用现场（疏），本模块负责堵。
//! （planner 预置占位：lib.rs 声明先行入库，B162 在本文件内实现，勿动 lib.rs——frozenPaths。）

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

const HOOKS_PATH: &str = ".githooks";
const REFERENCE_TRANSACTION_HOOK: &str = ".githooks/reference-transaction";
const REFERENCE_TRANSACTION_HOOK_BYTES: &[u8] =
    include_bytes!("../../../../.githooks/reference-transaction");

/// Pure policy result for a single reference update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Allow,
    Block { reason: String },
}

/// Additional fast-forward policy once a canonical root PASS has armed the
/// verdict barrier.  Topic refs are never affected, and orch's narrowly
/// marked authorized no-ff merge is the sole ordinary runtime bypass.
pub fn verdict_barrier_guard_verdict(
    refname: &str,
    pending_root_pass: bool,
    is_authorized_orch_merge: bool,
) -> GuardVerdict {
    if refname != "refs/heads/main" || !pending_root_pass || is_authorized_orch_merge {
        GuardVerdict::Allow
    } else {
        GuardVerdict::Block {
            reason: "blocked refs/heads/main advance while a root PASS awaits merge/seal"
                .to_string(),
        }
    }
}

/// Decide whether a reference update is safe for the primary checkout.
///
/// `old_is_ancestor_of_new=None` means that ancestry could not be established.
/// The hook treats that as a block for `main`, preserving the ledger rather
/// than failing open. Linked worktrees and explicit orch plumbing bypass the
/// primary-checkout policy before individual refs are classified.
pub fn main_guard_verdict(
    refname: &str,
    old_is_ancestor_of_new: Option<bool>,
    is_primary_worktree: bool,
    is_orch_context: bool,
) -> GuardVerdict {
    if is_orch_context || !is_primary_worktree {
        return GuardVerdict::Allow;
    }
    if refname == "refs/stash" {
        return GuardVerdict::Block {
            reason: "blocked refs/stash update in the primary worktree".to_string(),
        };
    }
    if refname != "refs/heads/main" {
        return GuardVerdict::Allow;
    }
    match old_is_ancestor_of_new {
        Some(true) => GuardVerdict::Allow,
        Some(false) => GuardVerdict::Block {
            reason: format!(
                "blocked non-fast-forward {refname} update: old SHA is not an ancestor of new SHA"
            ),
        },
        None => GuardVerdict::Block {
            reason: format!(
                "blocked {refname} update: old/new SHA ancestry is unknown (fail-closed)"
            ),
        },
    }
}

/// Render one append-only JSONL violation record without a trailing newline.
pub fn violation_line(refname: &str, old: &str, new: &str, kind: &str) -> String {
    let ts = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<unknown>".to_string());
    serde_json::json!({
        "ts": ts,
        "refname": refname,
        "old": old,
        "new": new,
        "kind": kind,
        "cwd": cwd,
    })
    .to_string()
}

fn configured_hooks_path(root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "--get", "core.hooksPath"])
        .output()
        .with_context(|| "git config core.hooksPath 启动失败")?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8(output.stdout)
                .context("git config core.hooksPath 输出不是 UTF-8")?
                .trim()
                .to_string(),
        )),
        Some(1) => Ok(None),
        _ => bail!(
            "git config core.hooksPath 读取失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn install_hooks_path(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--local", "core.hooksPath", HOOKS_PATH])
        .output()
        .with_context(|| "git config core.hooksPath 安装失败")?;
    if !output.status.success() {
        bail!(
            "git config core.hooksPath 安装失败({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn ensure_hook_script(root: &Path) -> Result<()> {
    let script = root.join(REFERENCE_TRANSACTION_HOOK);
    match fs::symlink_metadata(&script) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "main guard hook 必须是 regular non-symlink file: {}",
                    script.display()
                );
            }
            let actual = fs::read(&script)
                .with_context(|| format!("读取 main guard hook 失败: {}", script.display()))?;
            if actual != REFERENCE_TRANSACTION_HOOK_BYTES {
                bail!(
                    "main guard hook 内容与内置版本不一致，拒绝静默覆盖: {}",
                    script.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = script.parent().context("main guard hook 缺少父目录")?;
            fs::create_dir_all(parent)
                .with_context(|| format!("创建 hooks 目录失败: {}", parent.display()))?;
            fs::write(&script, REFERENCE_TRANSACTION_HOOK_BYTES)
                .with_context(|| format!("写入 main guard hook 失败: {}", script.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("检查 main guard hook 失败: {}", script.display()));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(&script)
            .with_context(|| format!("读取 main guard hook 权限失败: {}", script.display()))?;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(mode | 0o111);
            fs::set_permissions(&script, permissions).with_context(|| {
                format!("设置 main guard hook 可执行权限失败: {}", script.display())
            })?;
        }
    }
    Ok(())
}

/// Idempotently install the tracked main guard into `root`.
///
/// A repository that selected a different hooks path is not silently
/// overwritten. Missing script bytes and executable bits are repaired; an
/// existing script with unexpected content is rejected.
pub fn ensure_main_guard(root: &Path) -> Result<()> {
    let configured = configured_hooks_path(root)?;
    if let Some(current) = configured.as_deref() {
        if current != HOOKS_PATH {
            bail!("core.hooksPath 已指向 {current:?}，拒绝静默覆盖为 {HOOKS_PATH:?}");
        }
    }

    ensure_hook_script(root)?;
    if configured.is_none() {
        install_hooks_path(root)?;
    }

    let final_path = configured_hooks_path(root)?;
    if final_path.as_deref() != Some(HOOKS_PATH) {
        bail!(
            "main guard 安装后 core.hooksPath 校验失败：期望 {HOOKS_PATH:?}，实得 {final_path:?}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
        let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        orch_root.join("target/test-tmp").join(format!(
            "main-guard-real-{name}-{}-{seq}",
            std::process::id()
        ))
    }

    fn git_output(root: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .env_remove("ORCH_MAIN_GUARD_BYPASS")
            .env_remove("ORCH_MAIN_GUARD_CONTEXT")
            .output()
            .unwrap()
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = git_output(root, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "--quiet"]);
        git(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(root, &["config", "user.name", "Main Guard Test"]);
        git(
            root,
            &["config", "user.email", "main-guard@example.invalid"],
        );
    }

    fn commit_file(root: &Path, contents: &str, message: &str) -> String {
        fs::write(root.join("tracked.txt"), contents).unwrap();
        git(root, &["add", "tracked.txt"]);
        git(root, &["commit", "--quiet", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    fn violation_records(root: &Path) -> Vec<serde_json::Value> {
        let path = root.join("coordination/runtime/guard/violations.jsonl");
        match fs::read_to_string(path) {
            Ok(contents) => contents
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read violations: {error}"),
        }
    }

    fn event_line(
        event_id: &str,
        actor: &str,
        kind: &str,
        task_id: Option<&str>,
        round: &str,
        payload: serde_json::Value,
    ) -> String {
        let mut event = serde_json::json!({
            "eventId": event_id,
            "ts": "2026-01-01T00:00:00Z",
            "actor": actor,
            "type": kind,
            "round": round,
            "payload": payload,
        });
        if let Some(task_id) = task_id {
            event
                .as_object_mut()
                .unwrap()
                .insert("taskId".to_string(), task_id.into());
        }
        format!("{}\n", serde_json::to_string(&event).unwrap())
    }

    fn root_pass_line(
        event_id: &str,
        task_id: &str,
        attempt_id: &str,
        attempt_no: usize,
        round: &str,
        task_head: &str,
        main_head: &str,
    ) -> String {
        event_line(
            event_id,
            "verifier:root",
            "VerdictIssued",
            Some(task_id),
            round,
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": attempt_no,
                "collectCompletedEventId": "01TESTCOLLECT00000000000000",
                "evidence": [],
                "gates": [],
                "headSha": task_head,
                "implementerAgent": "executor-desktop",
                "irRevision": 1,
                "mainHeadSha": main_head,
                "reviews": [],
                "validationDigest": "a".repeat(64),
                "verdict": "PASS",
            }),
        )
    }

    fn merge_started_line(
        event_id: &str,
        verdict_event_id: &str,
        task_id: &str,
        attempt_id: &str,
        attempt_no: usize,
        round: &str,
        task_head: &str,
        main_head: &str,
    ) -> String {
        event_line(
            event_id,
            "runtime:orch",
            "MergeStarted",
            Some(task_id),
            round,
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": attempt_no,
                "collectCompletedEventId": "01TESTCOLLECT00000000000000",
                "headSha": task_head,
                "mainHeadSha": main_head,
                "verdictEventId": verdict_event_id,
            }),
        )
    }

    fn write_active_round(root: &Path, round: &str, ledger: &str) {
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::create_dir_all(root.join(format!("coordination/rounds/{round}"))).unwrap();
        fs::write(
            root.join("coordination/runtime/CURRENT-ROUND"),
            format!("{round}\n"),
        )
        .unwrap();
        fs::write(
            root.join(format!("coordination/rounds/{round}/events.jsonl")),
            ledger,
        )
        .unwrap();
    }

    fn blocked_commit(root: &Path, contents: &str, message: &str) -> std::process::Output {
        fs::write(root.join("tracked.txt"), contents).unwrap();
        git(root, &["add", "tracked.txt"]);
        let output = git_output(root, &["commit", "--quiet", "-m", message]);
        assert!(
            !output.status.success(),
            "guard unexpectedly allowed {message}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    #[test]
    fn installed_hook_blocks_real_reset_but_allows_commit_and_detached_worktree() {
        let root = temp_root("production");
        init_repo(&root);
        let first = commit_file(&root, "first\n", "first");
        let before_reset = commit_file(&root, "second\n", "second");

        ensure_main_guard(&root).unwrap();
        let script_before = fs::read(root.join(REFERENCE_TRANSACTION_HOOK)).unwrap();
        ensure_main_guard(&root).unwrap();
        assert_eq!(
            fs::read(root.join(REFERENCE_TRANSACTION_HOOK)).unwrap(),
            script_before
        );

        let reset = git_output(&root, &["reset", "--hard", "HEAD~1"]);
        assert!(
            !reset.status.success(),
            "non-fast-forward reset must be rejected"
        );
        let reset_stderr = String::from_utf8_lossy(&reset.stderr);
        assert!(reset_stderr.contains("refs/heads/main"), "{reset_stderr}");
        assert!(reset_stderr.contains(&before_reset), "{reset_stderr}");
        assert!(reset_stderr.contains(&first), "{reset_stderr}");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), before_reset);

        let violations = violation_records(&root);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0]["refname"], "refs/heads/main");
        assert_eq!(violations[0]["old"], before_reset);
        assert_eq!(violations[0]["new"], first);
        assert_eq!(violations[0]["kind"], "non-ff");

        let after_commit = commit_file(&root, "third\n", "normal fast-forward");
        assert_ne!(after_commit, before_reset);
        let linked = root.join("linked-detached");
        crate::gitx::worktree_add_detached(&root, &linked, &after_commit).unwrap();
        assert!(linked.join(".git").is_file());
        assert_eq!(violation_records(&root).len(), 1);
    }

    #[test]
    fn pending_root_pass_blocks_real_commit_but_exact_orch_merge_succeeds() {
        let root = temp_root("verdict-barrier");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        git(&root, &["checkout", "--quiet", "-b", "task/B900"]);
        let task_head = commit_file(&root, "task\n", "task");
        git(&root, &["checkout", "--quiet", "main"]);
        ensure_main_guard(&root).unwrap();

        let verdict_id = "01TESTVERDICT00000000000000";
        let verdict = root_pass_line(
            verdict_id,
            "B900",
            "B900-A0001",
            1,
            "r900",
            &task_head,
            &main_head,
        );
        write_active_round(&root, "r900", &verdict);
        let ledger = root.join("coordination/rounds/r900/events.jsonl");

        fs::write(root.join("tracked.txt"), "ordinary\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        let blocked = git_output(&root, &["commit", "--quiet", "-m", "must block"]);
        assert!(
            !blocked.status.success(),
            "pending PASS must block git commit"
        );
        let stderr = String::from_utf8_lossy(&blocked.stderr);
        assert!(stderr.contains("B900"), "{stderr}");
        assert!(stderr.contains("B900-A0001"), "{stderr}");
        assert!(stderr.contains(verdict_id), "{stderr}");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main_head);
        let violations = violation_records(&root);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0]["kind"], "verdict-barrier");
        assert!(violations[0]["reason"]
            .as_str()
            .unwrap()
            .contains("B900-A0001"));

        // Merely exporting the private marker does not authorize a one-parent
        // commit.  Authorization requires the durable six-key MergeStarted
        // binding and the exact two-parent merge shape.
        let fake_context = Command::new("git")
            .args(["commit", "--quiet", "-m", "marker alone must block"])
            .current_dir(&root)
            .env_remove("ORCH_MAIN_GUARD_BYPASS")
            .env("ORCH_MAIN_GUARD_CONTEXT", "authorized-no-ff-merge")
            .output()
            .unwrap();
        assert!(
            !fake_context.status.success(),
            "authorization marker alone must not permit a one-parent commit"
        );
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main_head);

        // Return the index/worktree to the pre-attempt bytes without invoking
        // reset/checkout on a path, then arm the exact merge authorization.
        fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        let started = merge_started_line(
            "01TESTSTARTED00000000000000",
            verdict_id,
            "B900",
            "B900-A0001",
            1,
            "r900",
            &task_head,
            &main_head,
        );
        fs::write(&ledger, format!("{verdict}{started}")).unwrap();
        let merged = Command::new("git")
            .args([
                "merge",
                "--no-ff",
                "--no-verify",
                "task/B900",
                "-m",
                "authorized merge",
            ])
            .current_dir(&root)
            .env_remove("ORCH_MAIN_GUARD_BYPASS")
            .env("ORCH_MAIN_GUARD_CONTEXT", "authorized-no-ff-merge")
            .output()
            .unwrap();
        assert!(
            merged.status.success(),
            "authorized merge failed: {}",
            String::from_utf8_lossy(&merged.stderr)
        );
        let parents = git(&root, &["rev-list", "--parents", "-n", "1", "HEAD"]);
        let parents = parents.split_whitespace().collect::<Vec<_>>();
        assert_eq!(parents.len(), 3);
        assert_eq!(parents[1], main_head);
        assert_eq!(parents[2], task_head);
        assert_eq!(violation_records(&root).len(), 2);
    }

    /// H99 (r62/B204): an attempt can be terminally blocked after its root PASS
    /// and before any `MergeStarted` — the executor files honest blocked
    /// evidence, or H98 voids the verdict through a compile-input change. That
    /// dead attempt can never reach merge, so its PASS must not hold main
    /// hostage for the rest of the round. The guard used to fail-closed on this
    /// shape and froze every `refs/heads/main` update, including the merge that
    /// installs this very hook.
    ///
    /// The three negative variants pin the bindings that keep this from being a
    /// weaker way past the barrier: the terminal has to name the pending
    /// attempt exactly (id, ordinal and agent), and an armed barrier is never
    /// released by a terminal alone.
    #[test]
    fn honest_terminal_before_merge_start_releases_a_dead_root_pass() {
        let root = temp_root("honest-terminal");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        git(&root, &["checkout", "--quiet", "-b", "task/B901"]);
        let task_head = commit_file(&root, "task\n", "task");
        git(&root, &["checkout", "--quiet", "main"]);
        ensure_main_guard(&root).unwrap();

        let verdict_id = "01TESTVERDICT00000000000000";
        let verdict = root_pass_line(
            verdict_id,
            "B901",
            "B901-A0001",
            1,
            "r901",
            &task_head,
            &main_head,
        );
        // r62 ledger lines 407-408 verbatim in shape: the escalation is emitted
        // under `runtime:orch` (it closes no barrier — there is none), and the
        // terminal carries the executor's blocked evidence.
        let escalation = event_line(
            "01TESTESCALATION0000000000",
            "runtime:orch",
            "EscalationRaised",
            Some("B901"),
            "r901",
            serde_json::json!({
                "blockedPath": ".worktrees/B901/reports/B901-BLOCKED.md",
                "hint": "处置路径：授权改卡后运行 orch nudge；或改派其他执行者",
                "reason": "执行者提交了本 attempt 的诚实阻塞证据",
                "stage": "executor-blocked",
            }),
        );
        let terminal = |attempt_id: &str, attempt_no: usize, agent: &str| {
            event_line(
                "01TESTBLOCKED00000000000000",
                "runtime:orch",
                "AttemptBlocked",
                Some("B901"),
                "r901",
                serde_json::json!({
                    "agent": agent,
                    "attemptId": attempt_id,
                    "attemptNo": attempt_no,
                    "blockedPath": ".worktrees/B901/reports/B901-BLOCKED.md",
                    "evidenceLen": 2698,
                }),
            )
        };

        // Negative 1: the terminal names a different ordinal than the pending
        // PASS — it does not describe this attempt, so the barrier holds.
        write_active_round(
            &root,
            "r901",
            &format!(
                "{verdict}{escalation}{}",
                terminal("B901-A0002", 2, "executor-desktop")
            ),
        );
        let stderr = String::from_utf8_lossy(
            &blocked_commit(&root, "mismatched ordinal\n", "must block").stderr,
        )
        .into_owned();
        // A terminal that names some other attempt leaves the guard unable to
        // decide, so it refuses rather than assuming the PASS is dead.
        assert!(
            stderr.contains("non-terminal AttemptBlocked cannot clear pending root PASS"),
            "{stderr}"
        );

        // Negative 2: right attempt, wrong agent. The sibling arms bind the
        // dispatched agent too; this one must not be laxer.
        write_active_round(
            &root,
            "r901",
            &format!(
                "{verdict}{escalation}{}",
                terminal("B901-A0001", 1, "executor-opencode")
            ),
        );
        blocked_commit(&root, "mismatched agent\n", "must block");

        // Negative 3: the barrier is armed. Once `MergeStarted` is on the
        // ledger the merge may really have run, so closure has to come from
        // `MergeExecuted` or an adjacent merge-conflict / barrier-recovered
        // pair — never from a terminal on its own.
        let started = merge_started_line(
            "01TESTSTARTED00000000000000",
            verdict_id,
            "B901",
            "B901-A0001",
            1,
            "r901",
            &task_head,
            &main_head,
        );
        write_active_round(
            &root,
            "r901",
            &format!(
                "{verdict}{started}{escalation}{}",
                terminal("B901-A0001", 1, "executor-desktop")
            ),
        );
        blocked_commit(&root, "armed barrier\n", "must block");

        // Positive: the exact r62/B204-A0003 shape releases main.
        write_active_round(
            &root,
            "r901",
            &format!(
                "{verdict}{escalation}{}",
                terminal("B901-A0001", 1, "executor-desktop")
            ),
        );
        fs::write(root.join("tracked.txt"), "after honest terminal\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        let allowed = git_output(&root, &["commit", "--quiet", "-m", "ordinary work resumes"]);
        assert!(
            allowed.status.success(),
            "a dead attempt must not freeze main: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        assert_ne!(git(&root, &["rev-parse", "HEAD"]), main_head);
    }

    #[test]
    fn merge_barrier_closure_requires_adjacent_attempt_terminal() {
        let root = temp_root("closure-pair");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        git(&root, &["checkout", "--quiet", "-b", "task/B906"]);
        let task_head = commit_file(&root, "task\n", "task");
        git(&root, &["checkout", "--quiet", "main"]);
        ensure_main_guard(&root).unwrap();

        let verdict_id = "01TESTPAIRVERDICT0000000000";
        let verdict = root_pass_line(
            verdict_id,
            "B906",
            "B906-A0001",
            1,
            "r906",
            &task_head,
            &main_head,
        );
        let started = merge_started_line(
            "01TESTPAIRSTARTED0000000000",
            verdict_id,
            "B906",
            "B906-A0001",
            1,
            "r906",
            &task_head,
            &main_head,
        );
        let closure = event_line(
            "01TESTPAIRCLOSURE0000000000",
            "reviewer:orch-runtime",
            "EscalationRaised",
            Some("B906"),
            "r906",
            serde_json::json!({
                "stage": "merge-conflict",
                "mergeSha": null,
                "conflictFiles": ["tracked.txt"],
            }),
        );
        write_active_round(&root, "r906", &format!("{verdict}{started}{closure}"));

        let orphan = blocked_commit(&root, "orphan\n", "orphan closure must block");
        assert!(
            String::from_utf8_lossy(&orphan.stderr).contains("adjacent exact AttemptBlocked"),
            "{}",
            String::from_utf8_lossy(&orphan.stderr)
        );
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main_head);

        let mismatched_terminal = event_line(
            "01TESTPAIRMISMATCH000000000",
            "runtime:orch",
            "AttemptBlocked",
            Some("B906"),
            "r906",
            serde_json::json!({
                "attemptId": "B906-A0002",
                "attemptNo": 2,
                "agent": "executor-desktop",
                "stage": "merge-conflict",
                "reason": "refs did not move",
            }),
        );
        write_active_round(
            &root,
            "r906",
            &format!("{verdict}{started}{closure}{mismatched_terminal}"),
        );
        let mismatch = blocked_commit(&root, "mismatch\n", "mismatched closure must block");
        assert!(
            String::from_utf8_lossy(&mismatch.stderr).contains("adjacent exact AttemptBlocked"),
            "{}",
            String::from_utf8_lossy(&mismatch.stderr)
        );
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main_head);

        let terminal = event_line(
            "01TESTPAIRTERMINAL000000000",
            "runtime:orch",
            "AttemptBlocked",
            Some("B906"),
            "r906",
            serde_json::json!({
                "attemptId": "B906-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "stage": "merge-conflict",
                "reason": "refs did not move",
            }),
        );
        write_active_round(
            &root,
            "r906",
            &format!("{verdict}{started}{closure}{terminal}"),
        );
        let closed = commit_file(&root, "closed\n", "exact closure pair allows commit");
        assert_ne!(closed, main_head);
        assert_eq!(violation_records(&root).len(), 2);
    }

    #[test]
    fn approved_reattempt_reason_accepts_valid_json_escapes() {
        let root = temp_root("escaped-reason");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        git(&root, &["checkout", "--quiet", "-b", "task/B907"]);
        let task_head = commit_file(&root, "task\n", "task");
        git(&root, &["checkout", "--quiet", "main"]);
        ensure_main_guard(&root).unwrap();

        let verdict_id = "01TESTESCAPEDVERDICT00000000";
        let verdict = root_pass_line(
            verdict_id,
            "B907",
            "B907-A0001",
            1,
            "r907",
            &task_head,
            &main_head,
        );
        let terminal = event_line(
            "01TESTESCAPEDTERMINAL0000000",
            "runtime:orch",
            "AttemptBlocked",
            Some("B907"),
            "r907",
            serde_json::json!({
                "attemptId": "B907-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "stage": "approved-reattempt",
                "verdictEventId": verdict_id,
                "reason": "planner said \"retry\" under C:\\work",
            }),
        );
        write_active_round(&root, "r907", &format!("{verdict}{terminal}"));
        let committed = commit_file(&root, "escaped\n", "escaped reason stays operable");
        assert_ne!(committed, main_head);
        assert!(violation_records(&root).is_empty());
    }

    #[test]
    fn strict_guard_blocks_bad_pointer_and_truncated_active_ledger() {
        let bad_pointer_root = temp_root("bad-pointer");
        init_repo(&bad_pointer_root);
        let bad_pointer_head = commit_file(&bad_pointer_root, "base\n", "base");
        ensure_main_guard(&bad_pointer_root).unwrap();
        fs::create_dir_all(bad_pointer_root.join("coordination/runtime")).unwrap();
        fs::write(
            bad_pointer_root.join("coordination/runtime/CURRENT-ROUND"),
            "r0900\n",
        )
        .unwrap();
        fs::write(bad_pointer_root.join("tracked.txt"), "blocked\n").unwrap();
        git(&bad_pointer_root, &["add", "tracked.txt"]);
        let bad_pointer = git_output(
            &bad_pointer_root,
            &["commit", "--quiet", "-m", "bad pointer must block"],
        );
        assert!(!bad_pointer.status.success());
        assert!(
            String::from_utf8_lossy(&bad_pointer.stderr).contains("r<positive integer>"),
            "{}",
            String::from_utf8_lossy(&bad_pointer.stderr)
        );
        assert_eq!(
            git(&bad_pointer_root, &["rev-parse", "HEAD"]),
            bad_pointer_head
        );

        let truncated_root = temp_root("truncated-ledger");
        init_repo(&truncated_root);
        let truncated_head = commit_file(&truncated_root, "base\n", "base");
        ensure_main_guard(&truncated_root).unwrap();
        let prefix = event_line(
            "01TESTROUNDOPENED00000000000",
            "runtime:orch",
            "RoundOpened",
            None,
            "r901",
            serde_json::json!({"purpose": "test"}),
        );
        let verdict = root_pass_line(
            "01TESTTRUNCATED000000000000",
            "B901",
            "B901-A0001",
            1,
            "r901",
            &"b".repeat(40),
            &truncated_head,
        );
        let truncated = format!("{prefix}{}", verdict.trim_end_matches('\n'));
        write_active_round(&truncated_root, "r901", &truncated);
        fs::write(truncated_root.join("tracked.txt"), "blocked\n").unwrap();
        git(&truncated_root, &["add", "tracked.txt"]);
        let blocked = git_output(
            &truncated_root,
            &["commit", "--quiet", "-m", "truncated ledger must block"],
        );
        assert!(!blocked.status.success());
        assert!(
            String::from_utf8_lossy(&blocked.stderr).contains("truncated"),
            "{}",
            String::from_utf8_lossy(&blocked.stderr)
        );
        assert_eq!(git(&truncated_root, &["rev-parse", "HEAD"]), truncated_head);
    }

    #[test]
    fn strict_guard_rejects_missing_empty_bad_duplicate_and_wrong_round_ledgers() {
        let root = temp_root("bad-ledgers");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        ensure_main_guard(&root).unwrap();
        let valid = event_line(
            "01TESTLEDGERVALID0000000000",
            "runtime:orch",
            "RoundOpened",
            None,
            "r902",
            serde_json::json!({"purpose": "test"}),
        );
        let duplicate = format!("{valid}{valid}");
        let wrong_round = event_line(
            "01TESTWRONGROUND0000000000",
            "runtime:orch",
            "RoundOpened",
            None,
            "r903",
            serde_json::json!({"purpose": "test"}),
        );
        let cases = [
            ("missing", None),
            ("empty", Some(String::new())),
            ("bad-json", Some("not-json\n".to_string())),
            ("duplicate-id", Some(duplicate)),
            ("wrong-round", Some(wrong_round)),
        ];
        let case_count = cases.len();
        for (index, (name, contents)) in cases.into_iter().enumerate() {
            write_active_round(
                &root,
                "r902",
                contents.as_deref().unwrap_or("placeholder\n"),
            );
            let ledger = root.join("coordination/rounds/r902/events.jsonl");
            if contents.is_none() {
                fs::remove_file(&ledger).unwrap();
            }
            fs::write(root.join("tracked.txt"), format!("blocked-{index}\n")).unwrap();
            git(&root, &["add", "tracked.txt"]);
            let message = format!("{name} ledger must block");
            let blocked = git_output(&root, &["commit", "--quiet", "-m", &message]);
            assert!(
                !blocked.status.success(),
                "{name} active ledger unexpectedly allowed main advance"
            );
            assert_eq!(git(&root, &["rev-parse", "HEAD"]), main_head);
        }
        assert_eq!(violation_records(&root).len(), case_count);
    }

    #[test]
    fn strict_guard_rejects_forged_barrier_shapes_and_separator_identities() {
        let root = temp_root("forged-barriers");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        ensure_main_guard(&root).unwrap();

        let unsafe_identity = event_line(
            "unsafe|event",
            "runtime:orch",
            "RoundOpened",
            None,
            "r905",
            serde_json::json!({"purpose": "test"}),
        );
        write_active_round(&root, "r905", &unsafe_identity);
        let unsafe_blocked = blocked_commit(&root, "unsafe\n", "separator id must block");
        assert!(
            String::from_utf8_lossy(&unsafe_blocked.stderr).contains("unsafe identity"),
            "{}",
            String::from_utf8_lossy(&unsafe_blocked.stderr)
        );

        let malformed_root = event_line(
            "01TESTMALFORMEDROOT00000000",
            "verifier:root",
            "VerdictIssued",
            Some("B905"),
            "r905",
            serde_json::json!({
                "attemptId": "B905-A0001",
                "headSha": "b".repeat(40),
                "verdict": "PASS",
            }),
        );
        write_active_round(&root, "r905", &malformed_root);
        let malformed_blocked = blocked_commit(&root, "malformed\n", "partial root must block");
        assert!(
            String::from_utf8_lossy(&malformed_blocked.stderr).contains("lacks required keys"),
            "{}",
            String::from_utf8_lossy(&malformed_blocked.stderr)
        );

        let verdict_id = "01TESTFORGEDVERDICT000000000";
        let verdict = root_pass_line(
            verdict_id,
            "B905",
            "B905-A0001",
            1,
            "r905",
            &"b".repeat(40),
            &main_head,
        );
        let extra_key_started = event_line(
            "01TESTEXTRAKEYSTART000000000",
            "runtime:orch",
            "MergeStarted",
            Some("B905"),
            "r905",
            serde_json::json!({
                "attemptId": "B905-A0001",
                "attemptNo": 1,
                "collectCompletedEventId": "01TESTCOLLECT00000000000000",
                "headSha": "b".repeat(40),
                "mainHeadSha": main_head,
                "verdictEventId": verdict_id,
                "unexpected": true,
            }),
        );
        write_active_round(&root, "r905", &format!("{verdict}{extra_key_started}"));
        let extra_key_blocked = blocked_commit(&root, "extra\n", "extra merge key must block");
        assert!(
            String::from_utf8_lossy(&extra_key_blocked.stderr).contains("key set is not exact"),
            "{}",
            String::from_utf8_lossy(&extra_key_blocked.stderr)
        );

        let stale_main = "c".repeat(40);
        let stale_verdict = root_pass_line(
            verdict_id,
            "B905",
            "B905-A0001",
            1,
            "r905",
            &"b".repeat(40),
            &stale_main,
        );
        let stale_started = merge_started_line(
            "01TESTSTALESTARTED0000000000",
            verdict_id,
            "B905",
            "B905-A0001",
            1,
            "r905",
            &"b".repeat(40),
            &stale_main,
        );
        write_active_round(&root, "r905", &format!("{stale_verdict}{stale_started}"));
        let stale_blocked = blocked_commit(&root, "stale\n", "stale main binding must block");
        assert!(
            String::from_utf8_lossy(&stale_blocked.stderr).contains("transaction old SHA"),
            "{}",
            String::from_utf8_lossy(&stale_blocked.stderr)
        );
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), main_head);
        assert_eq!(violation_records(&root).len(), 4);
    }

    #[test]
    fn pending_barrier_allows_topic_refs_but_linked_worktrees_cannot_move_shared_main() {
        let root = temp_root("topic-linked");
        init_repo(&root);
        let main_head = commit_file(&root, "base\n", "base");
        git(&root, &["checkout", "--quiet", "-b", "task/B904"]);
        let task_head = commit_file(&root, "task\n", "task");
        git(&root, &["checkout", "--quiet", "main"]);
        ensure_main_guard(&root).unwrap();
        let verdict = root_pass_line(
            "01TESTTOPICLINKED0000000000",
            "B904",
            "B904-A0001",
            1,
            "r904",
            &task_head,
            &main_head,
        );
        write_active_round(&root, "r904", &verdict);

        git(&root, &["update-ref", "refs/heads/topic-copy", &task_head]);
        assert_eq!(
            git(&root, &["rev-parse", "refs/heads/topic-copy"]),
            task_head
        );

        let linked = root.join("linked-detached");
        crate::gitx::worktree_add_detached(&root, &linked, &main_head).unwrap();
        fs::write(linked.join("linked-only.txt"), "linked\n").unwrap();
        git(&linked, &["add", "linked-only.txt"]);
        git(&linked, &["commit", "--quiet", "-m", "linked commit"]);
        let linked_head = git(&linked, &["rev-parse", "HEAD"]);
        ensure_main_guard(&linked).unwrap();

        // A linked checkout remains free to update topic refs even when its
        // coordination view is unusable.
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "broken\n").unwrap();
        git(
            &linked,
            &["update-ref", "refs/heads/linked-topic", &linked_head],
        );
        assert_eq!(
            git(&root, &["rev-parse", "refs/heads/linked-topic"]),
            linked_head
        );

        // refs/heads/main is shared across all worktrees, so a linked
        // checkout must not bypass the primary-main barrier.  Its local
        // ledger cannot safely authorize that repository-wide mutation.
        let blocked = git_output(
            &linked,
            &["update-ref", "refs/heads/main", &linked_head, &main_head],
        );
        assert!(!blocked.status.success());
        assert!(
            String::from_utf8_lossy(&blocked.stderr).contains("linked worktree"),
            "{}",
            String::from_utf8_lossy(&blocked.stderr)
        );
        assert_eq!(git(&root, &["rev-parse", "refs/heads/main"]), main_head);
        assert!(violation_records(&root).is_empty());
        let linked_violations = violation_records(&linked);
        assert_eq!(linked_violations.len(), 1);
        assert_eq!(linked_violations[0]["refname"], "refs/heads/main");
        assert_eq!(linked_violations[0]["kind"], "linked-worktree");
    }

    #[test]
    fn installer_refuses_to_replace_an_existing_hooks_path() {
        let root = temp_root("custom-path");
        init_repo(&root);
        git(&root, &["config", "core.hooksPath", "custom-hooks"]);

        let error = ensure_main_guard(&root).unwrap_err().to_string();
        assert!(error.contains("custom-hooks"), "{error}");
        assert!(!root.join(REFERENCE_TRANSACTION_HOOK).exists());
    }
}
