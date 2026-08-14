//! `orch verify`：运行时 spawn 独立 verifier（fresh 会话，只喂契约不喂叙事——design/06 §4）。
//! 本切片 verifier 固定用 claude headless（--allowedTools Bash Read，已探针实证）；
//! 独立性=同工具族异会话，降级披露（degraded-disclosed）随事件落账。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use orch_core::{read_ledger, EventRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

use crate::{binding, card, gate, ledger, wake};

/// Parses a JSONL event stream and extracts the final verdict.
///
/// Scans for the **last** `{"type":"result",...}` event.  Earlier result
/// events are ignored entirely — only the final result matters.  The
/// `result` field must be a string; within it the **last non-empty,
/// trimmed line** must be exactly one of:
///   `VERDICT: PASS` | `VERDICT: FAIL` | `VERDICT: BLOCKED`
/// (case-sensitive, exact colon + single space).  The verdict must be
/// the last non-empty line — trailing narrative after it is rejected.
/// Duplicate or conflicting verdict lines within the same final result
/// are also rejected.
pub fn parse_verdict_stream(jsonl: &str) -> Result<String> {
    // Track the final result event's payload.  `None` = no result event seen
    // yet; `Some(Err)` = last result was non-string; `Some(Ok)` = string text.
    let mut last_result: std::result::Result<String, ()> = Err(());
    let mut seen_result = false;
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // tolerate non-JSON lines
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("result") {
            // Only the last result event survives; overwrite any prior.
            seen_result = true;
            match v.get("result").and_then(|r| r.as_str()) {
                Some(res) => last_result = Ok(res.to_string()),
                None => last_result = Err(()), // non-string → protocol error
            }
        }
        // non-result events (assistant, etc.) never replace the last result
    }

    if !seen_result {
        bail!("no result event found in verifier stream");
    }
    let raw = last_result
        .map_err(|_| anyhow::anyhow!("last result event's `result` field is not a string"))?;

    // Find the last non-empty, trimmed line.
    let last_line = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .last()
        .context("last result has no non-empty line")?;

    // Must be exactly one valid verdict form.
    let valid = ["VERDICT: PASS", "VERDICT: FAIL", "VERDICT: BLOCKED"];
    if !valid.contains(&last_line) {
        bail!("last non-empty line is not a valid verdict: {last_line:?}");
    }

    // Reject duplicate/conflicting verdicts: count verdict lines in the
    // full text.  Exactly one is allowed.
    let verdict_count = raw
        .lines()
        .filter(|l| {
            let t = l.trim();
            valid.contains(&t)
        })
        .count();
    if verdict_count != 1 {
        bail!("exactly one verdict line required, found {verdict_count}");
    }

    Ok(last_line.trim_start_matches("VERDICT: ").to_string())
}

pub struct VerifyOutcome {
    pub verdict: String,
    pub review_rel: String,
    pub duration_secs: u64,
    pub cost_usd: Option<f64>,
    pub usage: crate::cost::VerifyUsage,
}

fn command_text(b: &binding::Binding, command_ref: &str) -> Result<String> {
    let spec = b
        .commands
        .get(command_ref)
        .with_context(|| format!("绑定缺命令: {command_ref}"))?;
    Ok(spec.argv.join(" "))
}

fn render_verifier_prompt(
    root: &Path,
    round: &str,
    c: &card::Card,
    wt_abs: &str,
    review_rel: &str,
    b: &binding::Binding,
) -> Result<String> {
    let seeds = c
        .meta
        .seeds
        .iter()
        .map(|s| {
            format!(
                "  - 种子源 {}（头注释含负向变异清单）→ 落位 {}",
                s.src, s.target
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let report_rel = format!(
        "coordination/rounds/{round}/reports/{}-REPORT.md",
        c.meta.task_id
    );
    let test_fast = command_text(b, "testFast")?;
    let terminal_gates = c
        .meta
        .gates
        .fast
        .iter()
        .map(|command_ref| command_text(b, command_ref))
        .collect::<Result<Vec<_>>>()?
        .join(" && ");
    let node_modules_hint = b
        .has_ecosystem("node")
        .then_some("（untracked node_modules 软链可忽略）")
        .unwrap_or_default();
    Ok(format!(
        "你是独立 VERIFIER（fresh 会话）。只依据地面真值（文件/git/测试运行）复核——报告叙事不作证据，但必须被核对（E12）。\n\
         主仓根：{root}；被验 worktree：{wt}（分支 task/{id}）。任务卡：{card_rel}（先读它与种子头注释）。\n{seeds}\n\n\
         工具纪律：你仅有 Bash 与 Read 两个工具。一切文件写动作（变异注入/撤销/review 落盘）必须用 Bash（sed/python/heredoc）完成；Edit/Write 工具不可用，headless 无人应答权限请求——停下来请求任何授权即协议违约，直接用 Bash 做。\n\
         用 Bash 依次自查（运行时已机检过域/SHA/提交形状，你负责模型级复核）：\n\
         1. 终门：cd {wt} && {terminal_gates}（全部应成功）。\n\
         2. 先红复现核对：`git -C {wt} rev-list --reverse $(git -C {wt} merge-base main task/{id})..task/{id} | head -1` 定位种子搬运 commit → `git -C {wt} checkout <该SHA>`（detached）→ `{test_fast}`，记录 failed/passed 原文 → 与 (a) 种子头注释的 oracle 预言、(b) REPORT §3 声称的先红计数两相比对（E9 下界语义：附带殃及既有用例=正常，预言用例保持绿才违约）→ `git -C {wt} checkout task/{id}` 回分支头 → 复跑 `{test_fast}` 确认全绿。\n\
         3. 变异抽查：从种子头注释选第一条变异（M1），在 {wt} 内注入 → 跑 `{test_fast}` 确认预言用例红（下界语义）→ git checkout -- 复原 → 复跑确认全绿、git status 干净{node_modules_hint}。\n\
         4. 超规格守卫：git -C {root} diff $(git -C {root} merge-base main task/{id})..task/{id} -- <实现文件>，检查改动是否最小、有无契约外逻辑。\n\
         5. REPORT 叙事交叉核对：读 {root}/{report}，把 §3 声称的先红计数与你第 2 步的实测、§5 声称的变异结果与你第 3 步的实测逐项对照。**任何声称与地面真值不符 = 诚实性违约 → VERDICT: FAIL，即使实现本身正确**（执行者的可信度是验收对象的一部分）。\n\n\
         然后：把裁决写入 {root}/{review}（用 Bash heredoc 写；**此文件是必写产物，不写则裁决不落账、PASS 亦无效**。frontmatter: taskId/verdict/verifier: claude-headless/independence: degraded-disclosed，正文列各查项结论；若 FAIL 必须引用对不上的原文数字）。\n\
         最后一行回复必须是且仅是：VERDICT: PASS 或 VERDICT: FAIL 或 VERDICT: BLOCKED",
        root = root.display(),
        wt = wt_abs,
        id = c.meta.task_id,
        card_rel = c.rel_path,
        seeds = seeds,
        report = report_rel,
        review = review_rel,
        terminal_gates = terminal_gates,
        test_fast = test_fast,
        node_modules_hint = node_modules_hint,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_e12_checks() {
        // E12：verifier prompt 必含先红复现核对与 REPORT 叙事交叉核对（design/06 §4 清单）
        let meta: card::CardMeta = serde_yaml::from_str(
            "taskId: T1\nseeds:\n  - {src: s.rs, target: test/s.rs}\ngates:\n  fast: [testFast, check]\n",
        )
        .unwrap();
        let c = card::Card {
            meta,
            body: String::new(),
            rel_path: "coordination/rounds/r9/tasks/T1.md".into(),
        };
        let b: binding::Binding = serde_yaml::from_str(
            "project:\n  ecosystems: [rust]\ncommands:\n  testFast:\n    argv: [cargo, test]\n  check:\n    argv: [cargo, check]\n",
        )
        .unwrap();
        let p = render_verifier_prompt(
            Path::new("/repo"),
            "r9",
            &c,
            "/repo/.worktrees/T1",
            "coordination/rounds/r9/reviews/T1.review.md",
            &b,
        )
        .unwrap();
        for needle in [
            "先红复现核对",
            "rev-list --reverse",
            "REPORT 叙事交叉核对",
            "诚实性违约",
            "coordination/rounds/r9/reports/T1-REPORT.md",
            "VERDICT: PASS 或 VERDICT: FAIL 或 VERDICT: BLOCKED",
            "cargo test",
            "cargo check",
        ] {
            assert!(p.contains(needle), "prompt 缺必含项: {needle}");
        }
        assert!(!p.contains("npx vitest"));
        assert!(!p.contains("node_modules"));
    }
}

pub fn run_verify(
    root: &Path,
    task_id: &str,
    model: &str,
    timeout_secs: u64,
) -> Result<VerifyOutcome> {
    let round = fs::read_to_string(root.join("coordination/runtime/CURRENT-ROUND"))
        .context("CURRENT-ROUND 缺失")?
        .trim()
        .to_string();
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)
        .with_context(|| format!("direct verify 读取账本失败: {}", ledger_path.display()))?;
    if !lr.bad_lines.is_empty() {
        bail!("direct verify 拒绝坏账本");
    }
    let active = crate::plan::require_active_round_ir(root, &round, &lr.events)?;
    if active.candidate.verification.mode == "root-manual-fixed-head"
        && active.candidate.verification.adapter == "root-manual"
    {
        bail!(
            "orch verify 在 root-manual-fixed-head 模式禁用；使用 orch verdict 的机械 fixed-HEAD 入口"
        );
    }
    let c = card::load(root, &round, task_id)?;
    let b = binding::load(root)?;
    let wt = root.join(".worktrees").join(task_id);
    if !wt.is_dir() {
        bail!("worktree 不存在: {}（先 run-task）", wt.display());
    }
    let review_rel = format!("coordination/rounds/{round}/reviews/{task_id}.review.md");
    let prompt =
        render_verifier_prompt(root, &round, &c, &wt.display().to_string(), &review_rel, &b)?;

    let log_dir = root.join("coordination/runtime/logs");
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("{task_id}-verify.jsonl"));
    let mut cmd = Command::new("claude");
    cmd.args([
        "--model",
        model,
        "-p",
        &prompt,
        "--output-format",
        "stream-json",
        "--verbose",
        "--allowedTools",
        "Bash",
        "Read",
    ])
    .current_dir(root)
    .stdin(Stdio::null())
    .stdout(Stdio::from(File::create(&log_path)?))
    .stderr(Stdio::from(File::create(
        log_dir.join(format!("{task_id}-verify.stderr.log")),
    )?));

    let start = Instant::now();
    let mut child = cmd.spawn().context("spawn claude verifier 失败")?;
    let status = match child.wait_timeout(Duration::from_secs(timeout_secs))? {
        Some(s) => s,
        None => {
            child.kill().ok();
            child.wait().ok();
            bail!("verifier 超时（>{timeout_secs}s）");
        }
    };
    if !status.success() {
        bail!(
            "verifier 进程失败（exit {:?}），日志 {}",
            status.code(),
            log_path.display()
        );
    }
    // Parse verdict + cost from the event stream via the shared parser.
    let text = fs::read_to_string(&log_path)?;
    let verdict = parse_verdict_stream(&text)?;
    let usage = crate::cost::parse_verify_usage(&text);
    let cost_usd = usage.total_cost_usd;
    if !root.join(&review_rel).is_file() {
        bail!("verifier 未写 review 文件: {review_rel}（裁决 {verdict} 不落账）");
    }
    ledger::append(
        root,
        &round,
        &[ledger::event(
            "VerdictIssued",
            "verifier:claude-headless",
            Some(task_id),
            Some(&round),
            serde_json::json!({
                "verdict": verdict,
                "independence": "degraded-disclosed",
                "reviewPath": review_rel,
                "costUsd": cost_usd,
                "modelUsage": usage.per_model.iter().map(|entry| (
                    entry.model.clone(), serde_json::json!({
                        "inputTokens": entry.usage.input,
                        "outputTokens": entry.usage.output,
                        "cacheReadInputTokens": entry.usage.cache_read,
                        "cacheCreationInputTokens": entry.usage.cache_write,
                        "costUSD": entry.cost_usd,
                    })
                )).collect::<serde_json::Map<_, _>>(),
                "rateLimit": usage.rate_limit.as_ref().map(|rate| serde_json::json!({
                    "utilization": rate.utilization,
                    "status": rate.status,
                    "resetsAt": rate.resets_at,
                })),
            }),
        )],
    )?;
    Ok(VerifyOutcome {
        verdict,
        review_rel,
        duration_secs: start.elapsed().as_secs(),
        cost_usd,
        usage,
    })
}

// ───────────────── r48 · root-manual fixed-HEAD verdict ─────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootVerdict {
    Pass,
    Fail,
    Blocked,
}

impl RootVerdict {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pass" | "PASS" => Ok(Self::Pass),
            "fail" | "FAIL" => Ok(Self::Fail),
            "blocked" | "BLOCKED" => Ok(Self::Blocked),
            _ => bail!("--verdict 必须为 pass|fail|blocked"),
        }
    }

    pub fn as_event_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Blocked => "BLOCKED",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewBinding {
    pub path: String,
    pub role: String,
    pub reviewer: String,
    pub verdict: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceBinding {
    pub id: String,
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerdictGateBinding {
    pub name: String,
    pub exit_code: i32,
    pub log_sha256: String,
    pub log_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootVerdictPayload {
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub ir_revision: u32,
    pub validation_digest: String,
    pub attempt_id: String,
    pub attempt_no: usize,
    pub implementer_agent: String,
    pub head_sha: String,
    pub main_head_sha: String,
    pub collect_completed_event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_pre_signoff_attempt: Option<String>,
    pub reviews: Vec<ReviewBinding>,
    pub evidence: Vec<EvidenceBinding>,
    pub gates: Vec<VerdictGateBinding>,
}

pub struct RootVerdictOutcome {
    pub appended: bool,
    pub dry_run: bool,
    pub verdict: String,
    pub gates: Vec<gate::GateResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewBindingAuditLevel {
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBindingAuditFinding {
    pub level: ReviewBindingAuditLevel,
    pub round: String,
    pub task_id: String,
    pub attempt_id: String,
    pub path: String,
    pub bound_sha256: String,
    pub current_sha256: String,
    pub bound_bytes: u64,
    pub current_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewBindingAuditReport {
    pub checked_bindings: usize,
    pub legacy_tasks_skipped: usize,
    pub findings: Vec<ReviewBindingAuditFinding>,
}

impl std::fmt::Debug for RootVerdictOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootVerdictOutcome")
            .field("appended", &self.appended)
            .field("dry_run", &self.dry_run)
            .field("verdict", &self.verdict)
            .field("gates", &self.gates.len())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RootMergeAuthorization {
    pub verdict_event_id: String,
    pub attempt_id: String,
    pub attempt_no: usize,
    pub implementer_agent: String,
    pub head_sha: String,
    pub main_head_sha: String,
    pub collect_completed_event_id: String,
    pub bound_artifacts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecordAuthorization {
    pub verdict_event_id: String,
    pub attempt_id: String,
    pub attempt_no: usize,
    pub implementer_agent: String,
    pub head_sha: String,
    pub expected_main_sha: String,
    pub merge_sha: String,
    pub ir_revision: u32,
    pub validation_digest: String,
    pub already_recorded: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MergeStartedPayload {
    attempt_id: String,
    attempt_no: usize,
    head_sha: String,
    main_head_sha: String,
    collect_completed_event_id: String,
    verdict_event_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MergeExecutedPayload {
    merge_sha: String,
    policy: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskRecordedPayload {
    post_merge_gates: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MergeBoundaryShapePayload {
    stage: String,
    merge_sha: Option<String>,
    actual_main: String,
    actual_head: String,
    reason: String,
    hint: String,
}

#[derive(Debug, Deserialize)]
struct ReviewFrontmatter {
    #[serde(rename = "taskId")]
    task_id: String,
    round: String,
    #[serde(rename = "attemptId")]
    attempt_id: String,
    role: String,
    reviewer: String,
    verdict: String,
    #[serde(rename = "reviewedHead")]
    reviewed_head: String,
    #[serde(flatten)]
    ignored: BTreeMap<String, serde_yaml::Value>,
}

/// Runtime-owned identity against which every review artifact is decoded.
///
/// The six identity fields are immutable. `verdict` remains reviewer-owned,
/// but is accepted only as one of `PASS|FAIL|BLOCKED` by the shared codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewContractExpectation {
    task_id: String,
    round: String,
    attempt_id: String,
    role: String,
    reviewer: String,
    reviewed_head: String,
}

impl ReviewContractExpectation {
    pub fn exact(
        task_id: impl Into<String>,
        round: impl Into<String>,
        attempt_id: impl Into<String>,
        role: impl Into<String>,
        reviewer: impl Into<String>,
        reviewed_head: impl Into<String>,
    ) -> Result<Self> {
        let expectation = Self {
            task_id: task_id.into(),
            round: round.into(),
            attempt_id: attempt_id.into(),
            role: role.into(),
            reviewer: reviewer.into(),
            reviewed_head: reviewed_head.into(),
        };
        card::validate_task_id(&expectation.task_id)?;
        for (field, value) in [
            ("round", expectation.round.as_str()),
            ("attemptId", expectation.attempt_id.as_str()),
            ("role", expectation.role.as_str()),
            ("reviewer", expectation.reviewer.as_str()),
        ] {
            if !safe_identity_component(value) {
                bail!("review contract {field} 不是安全的非空 identity component");
            }
        }
        let attempt_prefix = format!("{}-A", expectation.task_id);
        let digits = expectation
            .attempt_id
            .strip_prefix(&attempt_prefix)
            .filter(|digits| digits.len() >= 4 && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .context("review contract attemptId 必须绑定 taskId 的四位 canonical ordinal")?;
        let ordinal = digits
            .parse::<usize>()
            .context("review contract attemptId ordinal 超出范围")?;
        if ordinal == 0
            || expectation.attempt_id != format!("{}-A{ordinal:04}", expectation.task_id)
        {
            bail!("review contract attemptId ordinal 必须为正数");
        }
        full_sha(&expectation.reviewed_head, "review contract reviewedHead")?;
        Ok(expectation)
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn round(&self) -> &str {
        &self.round
    }

    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub fn reviewer(&self) -> &str {
        &self.reviewer
    }

    pub fn reviewed_head(&self) -> &str {
        &self.reviewed_head
    }

    pub(crate) fn artifact_relpath(&self) -> String {
        canonical_review_artifact_relpath(&self.round, &self.attempt_id, &self.role, &self.reviewer)
    }
}

pub(crate) fn canonical_review_artifact_relpath(
    round: &str,
    attempt_id: &str,
    role: &str,
    reviewer: &str,
) -> String {
    format!("coordination/rounds/{round}/reviews/{attempt_id}-{role}-{reviewer}.md")
}

/// A complete, identity-checked review frontmatter plus its delivery metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedReviewArtifact {
    verdict: String,
    ignored_fields: Vec<String>,
    substantive_body_len: usize,
}

impl CheckedReviewArtifact {
    pub fn verdict(&self) -> &str {
        &self.verdict
    }

    pub fn ignored_fields(&self) -> &[String] {
        &self.ignored_fields
    }

    pub fn substantive_body_len(&self) -> usize {
        self.substantive_body_len
    }
}

fn full_sha(value: &str, flag: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{flag} 必须是完整 40 位小写 hex SHA");
    }
    Ok(())
}

fn full_sha256(value: &str, field: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{field} 必须是完整 64 位小写 hex SHA-256");
    }
    Ok(())
}

fn safe_identity_component(value: &str) -> bool {
    crate::plan::safe_identity_component(value)
}

fn regular_file_bytes(root: &Path, path: &Path, label: &str) -> Result<Vec<u8>> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{label} path 不在仓库内: {}", path.display()))?;
    let validate_chain = || -> Result<fs::Metadata> {
        let mut cursor = root.to_path_buf();
        let mut target = None;
        for component in rel.components() {
            use std::path::Component;
            let Component::Normal(name) = component else {
                bail!("{label} path 含非法 component: {}", path.display());
            };
            cursor.push(name);
            let metadata = fs::symlink_metadata(&cursor)
                .with_context(|| format!("{label} 缺失或 stat 失败: {}", cursor.display()))?;
            if metadata.file_type().is_symlink() {
                bail!("{label} path 不得含 symlink: {}", cursor.display());
            }
            if cursor == path {
                if !metadata.file_type().is_file() {
                    bail!("{label} 必须是 regular file: {}", path.display());
                }
                target = Some(metadata);
            } else if !metadata.file_type().is_dir() {
                bail!("{label} parent 必须是真实目录: {}", cursor.display());
            }
        }
        target.context("required artifact path 为空")
    };

    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("解析仓库根失败: {}", root.display()))?;
    let parent = path
        .parent()
        .with_context(|| format!("{label} 缺 parent: {}", path.display()))?;
    let canonical_parent_before = fs::canonicalize(parent)
        .with_context(|| format!("解析 {label} parent 失败: {}", parent.display()))?;
    if !canonical_parent_before.starts_with(&canonical_root) {
        bail!("{label} parent 逃逸仓库根");
    }
    let path_before = validate_chain()?;
    let mut file =
        File::open(path).with_context(|| format!("打开 {label} 失败: {}", path.display()))?;
    let handle_before = file.metadata()?;
    if !same_file_identity(&path_before, &handle_before) {
        bail!("{label} 在 stat/open 间发生替换");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("读取 {label} 失败: {}", path.display()))?;
    let handle_after = file.metadata()?;
    let path_after = validate_chain()?;
    let canonical_parent_after = fs::canonicalize(parent)
        .with_context(|| format!("复核 {label} parent 失败: {}", parent.display()))?;
    if canonical_parent_after != canonical_parent_before
        || !same_file_identity(&handle_before, &handle_after)
        || !same_file_identity(&handle_after, &path_after)
        || handle_after.len() != bytes.len() as u64
    {
        bail!("{label} 读取期间 bytes/inode/path 发生漂移");
    }
    Ok(bytes)
}

/// Read a mutable review artifact without following symlinks or accepting a
/// non-regular/racing replacement. Absence is the sole waiting state here;
/// once a directory entry exists, every structural fault is loud.
fn suppress_legacy_daemon_canonical_scan(root: &Path, path: &Path, label: &str) -> Result<bool> {
    if label != "daemon review artifact" {
        return Ok(false);
    }

    // `serve.rs` is frozen on B204, so this helper is the migration choke
    // point for its legacy working-tree scanner. From r62 onward reviewers
    // write only to review-inbox and `orch review deliver` is the sole
    // promotion entry; accepting a mutable canonical path here would reopen
    // the exact overwrite race that staging closes. Historical rounds retain
    // their old behavior for replay compatibility.
    let rel = path.strip_prefix(root).with_context(|| {
        format!(
            "daemon review artifact escaped repository root: {}",
            path.display()
        )
    })?;
    let components = rel
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .context("daemon review artifact path contains a non-canonical component")?;
    let [coordination, rounds, round, reviews, filename] = components.as_slice() else {
        bail!("daemon review artifact path is not canonical");
    };
    if coordination != "coordination"
        || rounds != "rounds"
        || reviews != "reviews"
        || filename.is_empty()
        || !filename.ends_with(".md")
    {
        bail!("daemon review artifact path is not canonical");
    }
    let digits = round
        .strip_prefix('r')
        .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .context("daemon review artifact round is not canonical")?;
    let ordinal = digits
        .parse::<u64>()
        .context("daemon review artifact round ordinal is out of range")?;
    if ordinal == 0 || round != &format!("r{ordinal}") {
        bail!("daemon review artifact round is not canonical");
    }
    Ok(ordinal >= 62)
}

fn ensure_single_review_link(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} stat failed: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.file_type().is_file() && metadata.nlink() != 1 {
            bail!(
                "{label} must have exactly one hard link: {}",
                path.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn optional_review_artifact_bytes(
    root: &Path,
    path: &Path,
    label: &str,
) -> Result<Option<Vec<u8>>> {
    if suppress_legacy_daemon_canonical_scan(root, path, label)? {
        return Ok(None);
    }
    match fs::symlink_metadata(path) {
        Ok(_) => {
            ensure_single_review_link(path, label)?;
            let bytes = regular_file_bytes(root, path, label)?;
            ensure_single_review_link(path, label)?;
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{label} stat 失败: {}", path.display())),
    }
}

#[cfg(all(test, unix))]
mod review_hardlink_tests {
    use super::*;

    #[test]
    fn mutable_review_readers_reject_inbox_aliases_to_canonical_bytes() {
        let root = crate::util::test_scratch_dir("b204-review-hardlink-alias");
        let canonical =
            root.join("coordination/rounds/r62/reviews/B204-A0001-primary-executor-claw.md");
        let inbox =
            root.join("coordination/runtime/review-inbox/r62/B204-A0001-primary-executor-claw.md");
        fs::create_dir_all(canonical.parent().unwrap()).unwrap();
        fs::create_dir_all(inbox.parent().unwrap()).unwrap();
        fs::write(&canonical, b"verdict-bound bytes\n").unwrap();
        fs::hard_link(&canonical, &inbox).unwrap();

        for (path, label) in [
            (&canonical, "verdict-bound canonical review"),
            (&inbox, "staged review inbox artifact"),
        ] {
            let error = optional_review_artifact_bytes(&root, path, label)
                .unwrap_err()
                .to_string();
            assert!(error.contains("hard link"), "{error}");
        }

        fs::remove_file(&inbox).unwrap();
        assert_eq!(
            optional_review_artifact_bytes(&root, &canonical, "verdict-bound canonical review")
                .unwrap(),
            Some(b"verdict-bound bytes\n".to_vec())
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod daemon_review_scan_migration_tests {
    use super::*;

    #[test]
    fn b204_daemon_canonical_scan_is_legacy_only_and_malformed_rounds_fail_closed() {
        let root = crate::util::test_scratch_dir("b204-daemon-review-scan-migration");
        let r61 = root.join("coordination/rounds/r61/reviews/legacy.md");
        let r62 = root.join("coordination/rounds/r62/reviews/staged.md");
        let malformed = root.join("coordination/rounds/r062/reviews/ambiguous.md");
        for path in [&r61, &r62, &malformed] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"review bytes\n").unwrap();
        }

        assert_eq!(
            optional_review_artifact_bytes(&root, &r61, "daemon review artifact").unwrap(),
            Some(b"review bytes\n".to_vec())
        );
        assert_eq!(
            optional_review_artifact_bytes(&root, &r62, "daemon review artifact").unwrap(),
            None,
            "r62+ daemon must not bypass inbox promotion by scanning mutable canonical bytes"
        );
        assert_eq!(
            optional_review_artifact_bytes(&root, &r62, "staged review inbox artifact").unwrap(),
            Some(b"review bytes\n".to_vec()),
            "the migration guard is scoped only to the legacy daemon call site"
        );
        assert!(
            optional_review_artifact_bytes(&root, &malformed, "daemon review artifact").is_err(),
            "an ambiguous round must not fall back to legacy scanning"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.file_type() == right.file_type()
}

fn split_review_frontmatter(bytes: &[u8]) -> Result<Option<(&str, &str)>> {
    if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(None);
    }
    let text = std::str::from_utf8(bytes).context("review artifact 必须为 UTF-8")?;
    let Some(open_end) = text.find('\n') else {
        if text.trim_end_matches('\r') == "---" {
            return Ok(None);
        }
        return Ok(None);
    };
    if text[..open_end].trim_end_matches('\r') != "---" {
        return Ok(None);
    }

    let body_start = open_end + 1;
    let mut offset = body_start;
    for line in text[body_start..].split_inclusive('\n') {
        let next = offset + line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok(Some((&text[body_start..offset], &text[next..])));
        }
        offset = next;
    }
    if offset < text.len() && text[offset..].trim_end_matches('\r') == "---" {
        return Ok(Some((&text[body_start..offset], "")));
    }
    Ok(None)
}

/// Decode one review artifact with the single host-wide contract.
///
/// `Ok(None)` means bytes are empty or the frontmatter fence has not closed
/// yet. Once the closing delimiter exists, malformed YAML, missing required
/// fields, invalid verdict/head, or identity drift are loud errors.
pub fn check_review_artifact_contract(
    bytes: &[u8],
    expected: &ReviewContractExpectation,
) -> Result<Option<CheckedReviewArtifact>> {
    let Some((frontmatter, body)) = split_review_frontmatter(bytes)? else {
        return Ok(None);
    };
    let parsed: ReviewFrontmatter =
        serde_yaml::from_str(frontmatter).context("review frontmatter 解析失败")?;
    full_sha(&parsed.reviewed_head, "review frontmatter reviewedHead")?;
    if parsed.task_id != expected.task_id
        || parsed.round != expected.round
        || parsed.attempt_id != expected.attempt_id
        || parsed.role != expected.role
        || parsed.reviewer != expected.reviewer
        || parsed.reviewed_head != expected.reviewed_head
    {
        bail!("review frontmatter 与 runtime fixed-HEAD tuple 不匹配");
    }
    if !matches!(parsed.verdict.as_str(), "PASS" | "FAIL" | "BLOCKED") {
        bail!("review verdict 非 PASS|FAIL|BLOCKED");
    }
    Ok(Some(CheckedReviewArtifact {
        verdict: parsed.verdict,
        ignored_fields: parsed.ignored.into_keys().collect(),
        substantive_body_len: body.trim().len(),
    }))
}

pub(crate) fn report_ignored_review_fields(path: &str, fields: &[String]) {
    if !fields.is_empty() {
        eprintln!(
            "orch: WARNING review frontmatter ignored fields: path={path} fields={}",
            fields.join(",")
        );
    }
}

fn sha_binding(bytes: &[u8]) -> (String, u64) {
    (hex::encode(Sha256::digest(bytes)), bytes.len() as u64)
}

fn committed_regular_blob_bytes(
    root: &Path,
    commit: &str,
    rel: &str,
    label: &str,
) -> Result<Vec<u8>> {
    full_sha(commit, "artifact commit")?;
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-z", commit, "--", rel])
        .output()
        .with_context(|| format!("查询 {label} committed tree entry 失败: {rel}"))?;
    if !output.status.success() {
        bail!(
            "查询 {label} committed tree entry 失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let entries = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.len() != 1 {
        bail!("{label} 必须在 expected main 中恰好有一个 tracked entry: {rel}");
    }
    let entry = std::str::from_utf8(entries[0])
        .with_context(|| format!("{label} ls-tree entry 非 UTF-8"))?;
    let (header, entry_path) = entry
        .split_once('\t')
        .with_context(|| format!("{label} ls-tree entry 格式错误"))?;
    let mut header_fields = header.split_whitespace();
    let mode = header_fields.next().unwrap_or_default();
    let object_type = header_fields.next().unwrap_or_default();
    let object_id = header_fields.next().unwrap_or_default();
    if !matches!(mode, "100644" | "100755")
        || object_type != "blob"
        || object_id.len() != 40
        || entry_path != rel
    {
        bail!("{label} 必须是 expected main 中的 regular tracked blob: {rel}");
    }
    crate::gitx::show_bytes(root, commit, rel)
        .with_context(|| format!("读取 {label} committed blob 失败: {rel}"))
}

fn parse_strict_committed_ledger(bytes: &[u8], label: &str) -> Result<Vec<EventRecord>> {
    let text = std::str::from_utf8(bytes).with_context(|| format!("{label} 必须为 UTF-8"))?;
    if !text.ends_with('\n') {
        bail!("{label} 必须以完整 newline 结尾");
    }
    let mut events = Vec::new();
    for (index, line) in text.split_terminator('\n').enumerate() {
        if line.is_empty() || line.trim() != line {
            bail!("{label} 第 {} 行为空洞或含外围空白", index + 1);
        }
        let event: EventRecord = serde_json::from_str(line)
            .with_context(|| format!("{label} 第 {} 行非合法 EventRecord", index + 1))?;
        events.push(event);
    }
    Ok(events)
}

fn canonical_round_number(round: &str) -> Result<u64> {
    let digits = round
        .strip_prefix('r')
        .filter(|digits| !digits.is_empty())
        .with_context(|| format!("round id is not canonical: {round:?}"))?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("round id is not canonical: {round:?}");
    }
    let number = digits
        .parse::<u64>()
        .with_context(|| format!("round ordinal overflow: {round:?}"))?;
    if round != format!("r{number}") {
        bail!("round id is not canonical: {round:?}");
    }
    Ok(number)
}

enum CurrentMainReviewBlob {
    Missing,
    NonRegular(String),
    Regular(Vec<u8>),
}

fn current_main_review_blob(
    root: &Path,
    main_sha: &str,
    rel: &str,
) -> Result<CurrentMainReviewBlob> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "--full-tree", "-z", main_sha, "--", rel])
        .output()
        .with_context(|| format!("doctor git ls-tree failed for review binding: {rel}"))?;
    if !output.status.success() {
        bail!(
            "doctor git ls-tree failed for {rel} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        return Ok(CurrentMainReviewBlob::Missing);
    }
    if output.stdout.last() != Some(&0) {
        bail!("doctor git ls-tree output is not NUL terminated for {rel}");
    }
    let entries = output.stdout[..output.stdout.len() - 1]
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.len() != 1 {
        bail!(
            "doctor exact review path resolved to {} tree entries: {rel}",
            entries.len()
        );
    }
    let entry = entries[0];
    let tab = entry
        .iter()
        .position(|byte| *byte == b'\t')
        .context("doctor review tree entry lacks path separator")?;
    let header = std::str::from_utf8(&entry[..tab])
        .context("doctor review tree entry header is not UTF-8")?;
    let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || fields[1] != "blob" || &entry[tab + 1..] != rel.as_bytes() {
        bail!("doctor review tree entry shape/path is malformed: {rel}");
    }
    if !matches!(fields[0], "100644" | "100755") {
        return Ok(CurrentMainReviewBlob::NonRegular(fields[0].to_string()));
    }
    Ok(CurrentMainReviewBlob::Regular(
        crate::gitx::show_bytes(root, main_sha, rel)
            .with_context(|| format!("doctor cannot read current main review blob: {rel}"))?,
    ))
}

/// Resolve the one MergeStarted that is completed by the recorded task's
/// MergeExecuted. A task may retain older attempts whose merge barrier was
/// honestly closed without moving main (merge-conflict/barrier-recovered), so
/// task-wide `MergeStarted.len() == 1` is not a valid invariant.
fn final_recorded_merge_start(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    merge_position: usize,
    recorded_position: usize,
) -> Result<(usize, MergeStartedPayload)> {
    if merge_position >= recorded_position {
        bail!("recorded merge lifecycle order is invalid");
    }
    let mut starts = Vec::<(usize, MergeStartedPayload)>::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "MergeStarted" && event.task_id.as_deref() == Some(task_id)
    }) {
        if event.actor != "runtime:orch" || event.round.as_deref() != Some(round) {
            bail!("recorded MergeStarted envelope is not canonical");
        }
        let payload: MergeStartedPayload = serde_json::from_value(
            event
                .payload
                .clone()
                .context("recorded MergeStarted lacks payload")?,
        )
        .context("recorded MergeStarted payload is not canonical")?;
        if position >= recorded_position
            || !safe_identity_component(&payload.attempt_id)
            || payload.attempt_id != format!("{task_id}-A{:04}", payload.attempt_no)
            || payload.attempt_no == 0
            || payload.collect_completed_event_id.is_empty()
            || payload.verdict_event_id.is_empty()
        {
            bail!("recorded MergeStarted identity/order is not canonical");
        }
        full_sha(&payload.head_sha, "recorded MergeStarted.headSha")?;
        full_sha(&payload.main_head_sha, "recorded MergeStarted.mainHeadSha")?;
        starts.push((position, payload));
    }
    let final_index = starts
        .iter()
        .rposition(|(position, _)| *position < merge_position)
        .context("recorded MergeExecuted has no preceding MergeStarted")?;
    if final_index + 1 != starts.len() {
        bail!("recorded task has a MergeStarted after its canonical MergeExecuted");
    }

    for index in 0..final_index {
        let (start_position, start) = &starts[index];
        let (next_position, next) = &starts[index + 1];
        if start.attempt_id == next.attempt_id || start.attempt_no >= next.attempt_no {
            bail!("historical MergeStarted attempts are not strictly successive");
        }
        let mut closures = 0usize;
        for position in start_position + 1..*next_position {
            if canonical_historical_premerge_closure(events, position, round, task_id, start)? {
                closures += 1;
            }
        }
        if closures != 1 {
            bail!(
                "historical MergeStarted lacks exactly one explicit no-merge closure: task={task_id} attempt={} found={closures}",
                start.attempt_id
            );
        }
    }

    let (final_position, _) = &starts[final_index];
    let mut final_closures = 0usize;
    for position in final_position + 1..merge_position {
        if canonical_historical_premerge_closure(
            events,
            position,
            round,
            task_id,
            &starts[final_index].1,
        )? {
            final_closures += 1;
        }
    }
    if final_closures != 0 {
        bail!("final MergeStarted was closed before MergeExecuted");
    }
    Ok(starts.remove(final_index))
}

fn canonical_historical_premerge_closure(
    events: &[EventRecord],
    position: usize,
    round: &str,
    task_id: &str,
    started: &MergeStartedPayload,
) -> Result<bool> {
    let event = &events[position];
    if event.kind != "EscalationRaised" || event.task_id.as_deref() != Some(task_id) {
        return Ok(false);
    }
    let stage = event
        .payload
        .as_ref()
        .and_then(|payload| payload.get("stage"))
        .and_then(serde_json::Value::as_str);
    if !matches!(stage, Some("merge-conflict" | "barrier-recovered")) {
        return Ok(false);
    }
    if event.actor != "reviewer:orch-runtime" || event.round.as_deref() != Some(round) {
        bail!("historical merge-barrier closure envelope is not canonical");
    }
    let payload = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("historical merge-barrier closure payload is not an object")?;
    match stage {
        Some("merge-conflict") => {
            const KEYS: &[&str] = &["stage", "mergeSha", "conflictFiles"];
            if payload.len() != KEYS.len()
                || KEYS.iter().any(|key| !payload.contains_key(*key))
                || payload.get("mergeSha").is_none_or(|value| !value.is_null())
                || payload
                    .get("conflictFiles")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|files| {
                        files
                            .iter()
                            .any(|file| file.as_str().is_none_or(|path| path.is_empty()))
                    })
            {
                bail!("historical merge-conflict closure payload is not canonical");
            }
        }
        Some("barrier-recovered") => {
            if payload.len() != 1 {
                bail!("historical barrier-recovered closure payload is not canonical");
            }
        }
        _ => unreachable!(),
    }

    let terminal = events
        .get(position + 1)
        .context("historical merge-barrier closure lacks adjacent AttemptBlocked")?;
    if terminal.kind != "AttemptBlocked"
        || terminal.actor != "runtime:orch"
        || terminal.task_id.as_deref() != Some(task_id)
        || terminal.round.as_deref() != Some(round)
    {
        bail!("historical merge-barrier closure lacks adjacent canonical AttemptBlocked");
    }
    let terminal_payload = terminal
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("historical merge AttemptBlocked payload is not an object")?;
    const TERMINAL_KEYS: &[&str] = &["attemptId", "attemptNo", "agent", "stage", "reason"];
    if terminal_payload.len() != TERMINAL_KEYS.len()
        || TERMINAL_KEYS
            .iter()
            .any(|key| !terminal_payload.contains_key(*key))
        || terminal_payload
            .get("attemptId")
            .and_then(serde_json::Value::as_str)
            != Some(started.attempt_id.as_str())
        || terminal_payload
            .get("attemptNo")
            .and_then(serde_json::Value::as_u64)
            != Some(started.attempt_no as u64)
        || terminal_payload
            .get("agent")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|agent| !safe_identity_component(agent))
        || terminal_payload
            .get("stage")
            .and_then(serde_json::Value::as_str)
            != Some("merge-conflict")
        || terminal_payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|reason| reason.trim().is_empty())
    {
        bail!("historical merge AttemptBlocked does not bind its MergeStarted");
    }
    Ok(true)
}

fn validate_archived_required_binding_membership(
    reviews: &[ReviewBinding],
    evidence: &[EvidenceBinding],
    required_reviews: &[crate::card::RequiredReview],
    required_evidence: &[String],
) -> Result<()> {
    let required_review_slots = required_reviews
        .iter()
        .map(|required| (required.role.as_str(), required.agent.as_str()))
        .collect::<BTreeSet<_>>();
    let bound_review_slots = reviews
        .iter()
        .map(|binding| (binding.role.as_str(), binding.reviewer.as_str()))
        .collect::<BTreeSet<_>>();
    if required_review_slots.len() != required_reviews.len()
        || bound_review_slots.len() != reviews.len()
        || reviews.len() != required_reviews.len()
        || bound_review_slots != required_review_slots
    {
        bail!("archived root PASS review bindings 未 exact 匹配 signed requiredReviews");
    }

    let required_evidence_ids = required_evidence
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let bound_evidence_ids = evidence
        .iter()
        .map(|binding| binding.id.as_str())
        .collect::<BTreeSet<_>>();
    if required_evidence_ids.len() != required_evidence.len()
        || bound_evidence_ids.len() != evidence.len()
        || evidence.len() != required_evidence.len()
        || bound_evidence_ids != required_evidence_ids
    {
        bail!("archived root PASS evidence bindings 未 exact 匹配 signed requiredEvidence");
    }
    Ok(())
}

#[cfg(test)]
mod recorded_merge_start_tests {
    use super::*;

    const ROUND: &str = "r62";
    const TASK: &str = "B204T";

    fn started(attempt_no: usize) -> EventRecord {
        ledger::event(
            "MergeStarted",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": format!("{TASK}-A{attempt_no:04}"),
                "attemptNo": attempt_no,
                "headSha": format!("{:x}", attempt_no).repeat(40),
                "mainHeadSha": "a".repeat(40),
                "collectCompletedEventId": format!("collect-{attempt_no}"),
                "verdictEventId": format!("verdict-{attempt_no}"),
            }),
        )
    }

    fn conflict() -> EventRecord {
        ledger::event(
            "EscalationRaised",
            "reviewer:orch-runtime",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "stage": "merge-conflict",
                "mergeSha": null,
                "conflictFiles": ["conflict.rs"],
            }),
        )
    }

    fn blocked(attempt_no: usize) -> EventRecord {
        ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": format!("{TASK}-A{attempt_no:04}"),
                "attemptNo": attempt_no,
                "agent": "executor-desktop",
                "stage": "merge-conflict",
                "reason": "refs did not move",
            }),
        )
    }

    fn successful_history() -> Vec<EventRecord> {
        vec![
            started(1),
            conflict(),
            blocked(1),
            started(2),
            ledger::event(
                "MergeExecuted",
                "reviewer:orch-runtime",
                Some(TASK),
                Some(ROUND),
                serde_json::json!({"mergeSha": "b".repeat(40), "policy": "no-ff"}),
            ),
            ledger::event(
                "TaskRecorded",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                serde_json::json!({"postMergeGates": "all-green"}),
            ),
        ]
    }

    #[test]
    fn doctor_and_archived_chain_select_final_start_after_closed_conflict_attempt() {
        let events = successful_history();
        let (position, payload) = final_recorded_merge_start(&events, ROUND, TASK, 4, 5).unwrap();
        assert_eq!(position, 3);
        assert_eq!(payload.attempt_id, "B204T-A0002");
        assert_eq!(payload.attempt_no, 2);
    }

    #[test]
    fn historical_start_requires_adjacent_terminal_bound_to_same_attempt() {
        let mut missing = successful_history();
        missing.remove(2);
        assert!(final_recorded_merge_start(&missing, ROUND, TASK, 3, 4)
            .unwrap_err()
            .to_string()
            .contains("adjacent canonical AttemptBlocked"));

        let mut mismatched = successful_history();
        mismatched[2].payload.as_mut().unwrap()["attemptId"] = serde_json::json!("B204T-A0099");
        assert!(final_recorded_merge_start(&mismatched, ROUND, TASK, 4, 5)
            .unwrap_err()
            .to_string()
            .contains("does not bind"));
    }

    fn review(role: &str, reviewer: &str) -> ReviewBinding {
        ReviewBinding {
            path: format!("review-{role}-{reviewer}.md"),
            role: role.to_string(),
            reviewer: reviewer.to_string(),
            verdict: "PASS".to_string(),
            sha256: "a".repeat(64),
            bytes: 1,
        }
    }

    fn evidence(id: &str) -> EvidenceBinding {
        EvidenceBinding {
            id: id.to_string(),
            path: format!("evidence-{id}.json"),
            sha256: "b".repeat(64),
            bytes: 1,
        }
    }

    #[test]
    fn archived_bindings_exactly_match_signed_review_slots_and_evidence_ids() {
        let required_reviews = vec![
            crate::card::RequiredReview {
                role: "primary".to_string(),
                agent: "executor-claw".to_string(),
            },
            crate::card::RequiredReview {
                role: "secondary".to_string(),
                agent: "executor-opencode".to_string(),
            },
        ];
        let required_evidence = vec!["seal-chain".to_string(), "hook-barrier".to_string()];
        let reviews = vec![
            review("secondary", "executor-opencode"),
            review("primary", "executor-claw"),
        ];
        let evidence = vec![evidence("hook-barrier"), evidence("seal-chain")];
        validate_archived_required_binding_membership(
            &reviews,
            &evidence,
            &required_reviews,
            &required_evidence,
        )
        .unwrap();

        let missing_review = validate_archived_required_binding_membership(
            &reviews[..1],
            &evidence,
            &required_reviews,
            &required_evidence,
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing_review.contains("requiredReviews"),
            "{missing_review}"
        );

        let missing_evidence = validate_archived_required_binding_membership(
            &reviews,
            &evidence[..1],
            &required_reviews,
            &required_evidence,
        )
        .unwrap_err()
        .to_string();
        assert!(
            missing_evidence.contains("requiredEvidence"),
            "{missing_evidence}"
        );
    }
}

/// Recompute every modern, recorded root-PASS review binding against one
/// freshly captured `refs/heads/main` tree.  Only the one disclosed
/// r59/B181 byte drift is downgraded to Warn; every other modern mismatch is
/// a new failure regardless of round. Older schemas are counted explicitly.
pub fn audit_recorded_review_bindings(root: &Path) -> Result<ReviewBindingAuditReport> {
    let main_sha = crate::gitx::rev_parse(root, "refs/heads/main")
        .context("doctor cannot capture refs/heads/main")?;
    full_sha(&main_sha, "doctor current main")?;
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            &main_sha,
            "--",
            "coordination/rounds",
        ])
        .output()
        .context("doctor cannot enumerate current main round ledgers")?;
    if !output.status.success() {
        bail!(
            "doctor cannot enumerate current main round ledgers ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !output.stdout.is_empty() && output.stdout.last() != Some(&0) {
        bail!("doctor round ledger tree listing is not NUL terminated");
    }
    let mut ledgers = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            std::str::from_utf8(entry)
                .map(str::to_string)
                .context("doctor round ledger path is not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    ledgers
        .retain(|path| path.starts_with("coordination/rounds/") && path.ends_with("/events.jsonl"));
    ledgers.sort();

    let mut report = ReviewBindingAuditReport {
        checked_bindings: 0,
        legacy_tasks_skipped: 0,
        findings: Vec::new(),
    };
    for ledger_rel in ledgers {
        let round = ledger_rel
            .strip_prefix("coordination/rounds/")
            .and_then(|value| value.strip_suffix("/events.jsonl"))
            .context("doctor round ledger path shape drift")?;
        if round.contains('/') {
            bail!("doctor round ledger path has nested round id: {ledger_rel}");
        }
        let round_no = canonical_round_number(round)?;
        let bytes =
            committed_regular_blob_bytes(root, &main_sha, &ledger_rel, "doctor round ledger")?;
        let events =
            parse_strict_committed_ledger(&bytes, &format!("doctor current-main {round} ledger"))?;
        if round_no < 48 {
            report.legacy_tasks_skipped += events
                .iter()
                .filter(|event| {
                    event.kind == "TaskRecorded"
                        && event.round.as_deref() == Some(round)
                        && event.task_id.is_some()
                })
                .map(|event| event.task_id.as_deref().unwrap_or_default())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            continue;
        }
        let mut recorded = BTreeMap::<String, usize>::new();
        for (position, event) in events.iter().enumerate().filter(|(_, event)| {
            event.kind == "TaskRecorded" && event.round.as_deref() == Some(round)
        }) {
            let recorded_payload: TaskRecordedPayload = serde_json::from_value(
                event
                    .payload
                    .clone()
                    .context("doctor TaskRecorded lacks payload")?,
            )
            .context("doctor TaskRecorded payload is not canonical")?;
            if event.actor != "runtime:orch" || recorded_payload.post_merge_gates != "all-green" {
                bail!("doctor found malformed canonical TaskRecorded in {round}");
            }
            let task_id = event
                .task_id
                .as_ref()
                .filter(|task| !task.is_empty())
                .context("doctor TaskRecorded lacks taskId")?
                .clone();
            if recorded.insert(task_id.clone(), position).is_some() {
                bail!("doctor found duplicate TaskRecorded: round={round} task={task_id}");
            }
        }
        for (task_id, recorded_position) in recorded {
            // r62+ was created after the archived-chain contract stabilized;
            // enforce that complete proof. Older modern rounds retain their
            // historical signed-IR compatibility but still get exact local
            // lifecycle uniqueness below.
            if round_no >= 62 {
                validate_archived_record_chain(root, round, &task_id, &events).with_context(
                    || format!("doctor archived record chain failed: {round}/{task_id}"),
                )?;
            }
            let merges = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.kind == "MergeExecuted"
                        && event.task_id.as_deref() == Some(task_id.as_str())
                })
                .map(|(position, event)| {
                    if event.actor != "reviewer:orch-runtime"
                        || event.round.as_deref() != Some(round)
                    {
                        bail!("doctor MergeExecuted envelope is not canonical");
                    }
                    let payload: MergeExecutedPayload = serde_json::from_value(
                        event
                            .payload
                            .clone()
                            .context("doctor MergeExecuted lacks payload")?,
                    )
                    .context("doctor MergeExecuted payload is not canonical")?;
                    Ok((position, payload))
                })
                .collect::<Result<Vec<_>>>()?;
            if merges.len() != 1 {
                bail!(
                    "doctor recorded task requires exactly one MergeExecuted: {round}/{task_id} found={}",
                    merges.len()
                );
            }
            let (merge_position, merged) = &merges[0];
            full_sha(&merged.merge_sha, "doctor MergeExecuted.mergeSha")?;
            if merged.policy != "no-ff" || *merge_position >= recorded_position {
                bail!("doctor MergeExecuted policy/order is not canonical");
            }
            let (started_position, started) = final_recorded_merge_start(
                &events,
                round,
                &task_id,
                *merge_position,
                recorded_position,
            )?;
            let verdicts = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.kind == "VerdictIssued"
                        && event.task_id.as_deref() == Some(task_id.as_str())
                        && event
                            .payload
                            .as_ref()
                            .and_then(|payload| payload.get("attemptId"))
                            .and_then(serde_json::Value::as_str)
                            == Some(started.attempt_id.as_str())
                })
                .map(|(position, event)| {
                    if event.actor != "verifier:root" || event.round.as_deref() != Some(round) {
                        bail!("doctor final-attempt VerdictIssued envelope is not canonical");
                    }
                    let payload: RootVerdictPayload = serde_json::from_value(
                        event
                            .payload
                            .clone()
                            .context("doctor root verdict missing payload")?,
                    )
                    .with_context(|| {
                        format!("doctor root verdict payload malformed: {round}/{task_id}")
                    })?;
                    Ok((position, event, payload))
                })
                .collect::<Result<Vec<_>>>()?;
            if verdicts.len() != 1 {
                bail!(
                    "doctor final attempt resolves {} root verdicts: {round}/{task_id}",
                    verdicts.len()
                );
            }
            let (verdict_position, verdict, payload) = &verdicts[0];
            if events
                .iter()
                .filter(|event| event.event_id == started.verdict_event_id)
                .count()
                != 1
                || verdict.event_id != started.verdict_event_id
                || *verdict_position >= started_position
                || payload.verdict != "PASS"
                || payload.reason.is_some()
                || payload.attempt_id != started.attempt_id
                || payload.attempt_no != started.attempt_no
                || payload.head_sha != started.head_sha
                || payload.main_head_sha != started.main_head_sha
                || payload.collect_completed_event_id != started.collect_completed_event_id
                || payload.gates.is_empty()
                || payload.gates.iter().any(|gate| gate.exit_code != 0)
            {
                bail!("doctor recorded chain does not reference exact root PASS attempt");
            }
            full_sha(&payload.head_sha, "doctor root PASS headSha")?;
            full_sha(&payload.main_head_sha, "doctor root PASS mainHeadSha")?;
            full_sha256(
                &payload.validation_digest,
                "doctor root PASS validationDigest",
            )?;
            if payload.reviews.is_empty() {
                bail!("doctor root PASS review bindings are empty");
            }
            let mut roles = BTreeSet::new();
            let mut reviewers = BTreeSet::new();
            let mut paths = BTreeSet::new();
            for binding in payload.reviews.clone() {
                full_sha256(&binding.sha256, "doctor bound review sha256")?;
                let expected = ReviewContractExpectation::exact(
                    &task_id,
                    round,
                    &payload.attempt_id,
                    &binding.role,
                    &binding.reviewer,
                    &payload.head_sha,
                )?;
                let canonical_rel = expected.artifact_relpath();
                if binding.path != canonical_rel
                    || !matches!(binding.role.as_str(), "primary" | "secondary")
                    || binding.verdict != "PASS"
                    || binding.reviewer == payload.implementer_agent
                    || binding.bytes == 0
                    || !roles.insert(binding.role.clone())
                    || !reviewers.insert(binding.reviewer.clone())
                    || !paths.insert(binding.path.clone())
                {
                    bail!(
                        "doctor root verdict review path is non-canonical: {}",
                        binding.path
                    );
                }
                report.checked_bindings += 1;
                let (current_sha256, current_bytes) =
                    match current_main_review_blob(root, &main_sha, &binding.path)? {
                        CurrentMainReviewBlob::Missing => ("<missing>".to_string(), None),
                        CurrentMainReviewBlob::NonRegular(mode) => {
                            (format!("<non-regular:{mode}>"), None)
                        }
                        CurrentMainReviewBlob::Regular(bytes) => {
                            let (sha256, len) = sha_binding(&bytes);
                            (sha256, Some(len))
                        }
                    };
                if current_sha256 != binding.sha256 || current_bytes != Some(binding.bytes) {
                    let disclosed_r59_b181_debt = round == "r59"
                        && task_id == "B181"
                        && payload.attempt_id == "B181-A0001"
                        && binding.path
                            == "coordination/rounds/r59/reviews/B181-A0001-primary-executor-claw.md"
                        && binding.sha256
                            == "a12f964e050a509769c4bbe65b1e85c83255f19965ef12120166acd524230c0d"
                        && binding.bytes == 5627
                        && current_sha256
                            == "4ba42d26b047c074ef87d180314fb929dbe2efde4f5303cf247a3e0dbd955944"
                        && current_bytes == Some(5636);
                    report.findings.push(ReviewBindingAuditFinding {
                        level: if disclosed_r59_b181_debt {
                            ReviewBindingAuditLevel::Warn
                        } else {
                            ReviewBindingAuditLevel::Fail
                        },
                        round: round.to_string(),
                        task_id: task_id.clone(),
                        attempt_id: payload.attempt_id.clone(),
                        path: binding.path,
                        bound_sha256: binding.sha256,
                        current_sha256,
                        bound_bytes: binding.bytes,
                        current_bytes,
                    });
                }
            }
        }
    }
    let fresh_main = crate::gitx::rev_parse(root, "refs/heads/main")
        .context("doctor cannot re-read refs/heads/main")?;
    if fresh_main != main_sha {
        bail!(
            "refs/heads/main moved during review binding audit: captured={} fresh={}",
            main_sha,
            fresh_main
        );
    }
    Ok(report)
}

fn event_values_equal(left: &[EventRecord], right: &[EventRecord]) -> Result<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.iter().zip(right) {
        if serde_json::to_value(left)? != serde_json::to_value(right)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn committed_binding(root: &Path, commit: &str) -> Result<binding::Binding> {
    let rel = "coordination/PROJECT-BINDING.yaml";
    let bytes = committed_regular_blob_bytes(root, commit, rel, "PROJECT-BINDING")?;
    let parsed: binding::Binding =
        serde_yaml::from_slice(&bytes).context("解析 expected main PROJECT-BINDING blob 失败")?;
    if root.join("Cargo.toml").is_file() || root.join("orch/Cargo.toml").is_file() {
        binding::validate_locked_rust_gates(&parsed).map_err(|errors| {
            anyhow::anyhow!("Rust 门 --locked 校验失败: {}", errors.join("; "))
        })?;
    }
    Ok(parsed)
}

#[derive(Clone, Copy)]
enum CommittedLedgerMode {
    Exact,
    CanonicalStorageSuffix,
    CanonicalVerdictSuffix,
    CanonicalRootSuffix,
    CanonicalPostMergeSuffix,
}

fn root_verdict_ledger_mode(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
) -> CommittedLedgerMode {
    if events.iter().any(|event| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
    }) {
        CommittedLedgerMode::CanonicalVerdictSuffix
    } else if events.iter().any(|event| {
        crate::ledger::canonical_gate_storage_audit_event(event)
            .is_some_and(|audit| audit.round == round)
    })
    {
        CommittedLedgerMode::CanonicalStorageSuffix
    } else {
        CommittedLedgerMode::Exact
    }
}

fn event_payload_str<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event.payload.as_ref()?.get(key)?.as_str()
}

fn event_payload_u32(event: &EventRecord, key: &str) -> Option<u32> {
    event
        .payload
        .as_ref()?
        .get(key)?
        .as_u64()?
        .try_into()
        .ok()
}

/// Authorize only the `SiteRetired` facts that the runtime appends beside a
/// task's canonical `TaskRecorded` fact.
///
/// `prior` is deliberately ordered ledger history: the expected-main prefix
/// followed by the already accepted part of its suffix.  That makes the
/// original lease visible while requiring the same-batch `TaskRecorded` anchor
/// to precede the retirement.  Every mismatch fails closed; the event kind by
/// itself grants no authority.
pub fn post_merge_site_retirement_is_authorized(
    event: &EventRecord,
    prior: &[EventRecord],
    round: &str,
    task_id: &str,
) -> bool {
    if event.kind != "SiteRetired"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(task_id)
        || event_payload_str(event, "taskId") != Some(task_id)
    {
        return false;
    }

    let (
        Some(site_id),
        Some(generation),
        Some(attempt_id),
        Some(role),
        Some(agent),
        Some(retire_event_id),
    ) = (
        event_payload_str(event, "siteId"),
        event_payload_u32(event, "generation"),
        event_payload_str(event, "attemptId"),
        event_payload_str(event, "role"),
        event_payload_str(event, "agent"),
        event_payload_str(event, "retireEventId"),
    )
    else {
        return false;
    };
    if generation == 0
        || [site_id, attempt_id, role, agent, retire_event_id]
            .iter()
            .any(|value| value.is_empty())
    {
        return false;
    }

    let same_site_identity = |candidate: &EventRecord| {
        event_payload_str(candidate, "siteId") == Some(site_id)
            && event_payload_u32(candidate, "generation") == Some(generation)
            && event_payload_str(candidate, "attemptId") == Some(attempt_id)
            && event_payload_str(candidate, "role") == Some(role)
            && event_payload_str(candidate, "agent") == Some(agent)
    };

    let task_records = prior
        .iter()
        .filter(|candidate| {
            candidate.kind == "TaskRecorded"
                && candidate.round.as_deref() == Some(round)
                && candidate.task_id.as_deref() == Some(task_id)
        })
        .collect::<Vec<_>>();
    if task_records.len() != 1 {
        return false;
    }
    let record = task_records[0];
    if record.event_id != retire_event_id || record.actor != "runtime:orch" {
        return false;
    }
    if prior
        .iter()
        .filter(|candidate| candidate.event_id == retire_event_id)
        .count()
        != 1
    {
        return false;
    }

    let matching_leases = prior
        .iter()
        .filter(|candidate| candidate.kind == "WorkspaceLeased" && same_site_identity(candidate))
        .collect::<Vec<_>>();
    if matching_leases.len() != 1 {
        return false;
    }
    let lease = matching_leases[0];
    if lease.actor != "runtime:orch"
        || lease.round.as_deref() != Some(round)
        || lease.task_id.as_deref() != Some(task_id)
    {
        return false;
    }

    !prior
        .iter()
        .any(|candidate| candidate.kind == "SiteRetired" && same_site_identity(candidate))
}

#[cfg(test)]
mod b259_site_retirement_suffix_tests {
    use super::*;

    const ROUND: &str = "r70";
    const TASK: &str = "B259";
    const RECORD_ID: &str = "record-B259";

    fn event(kind: &str, payload: serde_json::Value) -> EventRecord {
        ledger::event(kind, "runtime:orch", Some(TASK), Some(ROUND), payload)
    }

    fn lease() -> EventRecord {
        event(
            "WorkspaceLeased",
            serde_json::json!({
                "siteId": "B259-primary-executor-zcode-g01",
                "generation": 1,
                "attemptId": "B259-A0001",
                "role": "primary",
                "agent": "executor-zcode",
            }),
        )
    }

    fn recorded() -> EventRecord {
        let mut event = event("TaskRecorded", serde_json::json!({"taskId": TASK}));
        event.event_id = RECORD_ID.to_string();
        event
    }

    fn retirement() -> EventRecord {
        event(
            "SiteRetired",
            serde_json::json!({
                "siteId": "B259-primary-executor-zcode-g01",
                "generation": 1,
                "taskId": TASK,
                "attemptId": "B259-A0001",
                "role": "primary",
                "agent": "executor-zcode",
                "trigger": "task-recorded",
                "retireEventId": RECORD_ID,
            }),
        )
    }

    #[test]
    fn a_site_identity_can_be_retired_at_most_once() {
        let first = retirement();
        let prior = vec![lease(), recorded(), first];
        assert!(!post_merge_site_retirement_is_authorized(
            &retirement(),
            &prior,
            ROUND,
            TASK,
        ));
    }

    #[test]
    fn duplicate_or_noncanonical_lease_never_anchors_retirement() {
        let canonical = vec![lease(), recorded()];
        assert!(post_merge_site_retirement_is_authorized(
            &retirement(),
            &canonical,
            ROUND,
            TASK,
        ));

        let mut duplicate = canonical.clone();
        duplicate.insert(1, lease());
        assert!(!post_merge_site_retirement_is_authorized(
            &retirement(),
            &duplicate,
            ROUND,
            TASK,
        ));

        let mut foreign = canonical;
        foreign[0].round = Some("r69".to_string());
        assert!(!post_merge_site_retirement_is_authorized(
            &retirement(),
            &foreign,
            ROUND,
            TASK,
        ));
    }
}

/// Budget admission may durably record a threshold immediately before an
/// approved re-attempt closes the pending root PASS.  That accounting fact is
/// not merge authority, but it is a legitimate expected-main suffix.  Keep
/// this predicate deliberately exact so an arbitrary task event cannot hide
/// behind the budget event name while the root-PASS barrier is armed.
fn canonical_budget_threshold_suffix(event: &EventRecord, round: &str) -> bool {
    if event.kind != "BudgetThresholdCrossed"
        || event.actor != "runtime:orch"
        || event.task_id.is_some()
        || event.round.as_deref() != Some(round)
    {
        return false;
    }
    let Some(payload) = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    const PAYLOAD_KEYS: &[&str] = &["kind", "pct", "spent"];
    if payload.len() != PAYLOAD_KEYS.len()
        || PAYLOAD_KEYS.iter().any(|key| !payload.contains_key(*key))
        || !matches!(
            payload.get("kind").and_then(serde_json::Value::as_str),
            Some("usd" | "wall" | "wake")
        )
        || !matches!(
            payload.get("pct").and_then(serde_json::Value::as_u64),
            Some(50 | 80 | 100)
        )
    {
        return false;
    }
    let Some(spent) = payload.get("spent").and_then(serde_json::Value::as_object) else {
        return false;
    };
    const SPENT_KEYS: &[&str] = &["usd", "wallMins", "wakes"];
    if spent.len() != SPENT_KEYS.len()
        || SPENT_KEYS.iter().any(|key| !spent.contains_key(*key))
        || !spent
            .get("usd")
            .and_then(serde_json::Value::as_f64)
            .is_some_and(|value| value.is_finite() && value >= 0.0)
        || spent
            .get("wallMins")
            .and_then(serde_json::Value::as_u64)
            .is_none()
        || spent
            .get("wakes")
            .and_then(serde_json::Value::as_u64)
            .is_none()
    {
        return false;
    }

    // r62 events carry the B149 initiator tuple; plannerWakeId is the sole
    // optional context key for this non-wake accounting event.
    if event.extra.len() < 2
        || event.extra.len() > 3
        || event.extra.keys().any(|key| {
            !matches!(
                key.as_str(),
                "initiatorKind" | "invocationMode" | "plannerWakeId"
            )
        })
        || !matches!(
            event
                .extra
                .get("initiatorKind")
                .and_then(serde_json::Value::as_str),
            Some("human-interactive" | "root-agent-operated" | "daemon-automatic" | "test-fixture")
        )
        || event
            .extra
            .get("invocationMode")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        || event
            .extra
            .get("plannerWakeId")
            .is_some_and(|value| value.as_str().is_none_or(|value| value.trim().is_empty()))
    {
        return false;
    }
    true
}

#[cfg(test)]
mod b204_budget_suffix_tests {
    use super::*;

    #[test]
    fn approved_reattempt_allows_only_exact_round_budget_accounting_suffix() {
        let event = ledger::event(
            "BudgetThresholdCrossed",
            "runtime:orch",
            None,
            Some("r62"),
            serde_json::json!({
                "kind": "wake",
                "pct": 50,
                "spent": {"usd": 1.25, "wallMins": 2, "wakes": 3},
            }),
        );
        assert!(canonical_budget_threshold_suffix(&event, "r62"));

        let mut tasked = event.clone();
        tasked.task_id = Some("B204".to_string());
        assert!(!canonical_budget_threshold_suffix(&tasked, "r62"));

        let mut bad_pct = event.clone();
        bad_pct.payload.as_mut().unwrap()["pct"] = serde_json::json!(51);
        assert!(!canonical_budget_threshold_suffix(&bad_pct, "r62"));

        let mut forged_extra = event;
        forged_extra
            .extra
            .insert("mergeAuthority".to_string(), serde_json::Value::Bool(true));
        assert!(!canonical_budget_threshold_suffix(&forged_extra, "r62"));
    }
}

fn canonical_wake_backend_receipt(
    event: &EventRecord,
    prior: &[EventRecord],
    round: &str,
) -> Result<bool> {
    if event.kind != "AgentEventReceived"
        || event_payload_str(event, "agentEvent") != Some("wake-backend-receipt")
    {
        return Ok(false);
    }
    let task_id = event
        .task_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .context("wake-backend-receipt suffix requires top-level taskId")?;
    if event.actor != "runtime:orch" || event.round.as_deref() != Some(round) {
        bail!("wake-backend-receipt suffix actor/round mismatch");
    }
    let payload = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("wake-backend-receipt payload must be an object")?;
    const KEYS: &[&str] = &[
        "agentEvent",
        "actionId",
        "wakeId",
        "continuationId",
        "attemptId",
        "agent",
        "providerKind",
        "requestMessageSha256",
        "renderedMessageSha256",
        "receiptKind",
        "requestSessionId",
        "observedSessionId",
        "logPath",
        "probeOffset",
        "probeEnd",
        "windowSha256",
        "backendState",
    ];
    if payload.len() != KEYS.len() || KEYS.iter().any(|key| !payload.contains_key(*key)) {
        bail!("wake-backend-receipt payload shape is not exact");
    }
    let required = |key: &str| -> Result<&str> {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("wake-backend-receipt {key} missing/empty"))
    };
    let wake_id = required("wakeId")?;
    if required("actionId")? != wake_id {
        bail!("wake-backend-receipt actionId must equal wakeId");
    }
    let continuation_id = required("continuationId")?;
    let attempt_id = required("attemptId")?;
    let agent = required("agent")?;
    let continuation_parts = continuation_id.split(':').collect::<Vec<_>>();
    let review_continuation = match continuation_parts.as_slice() {
        ["review", continuation_round, continuation_task, continuation_attempt, role, continuation_agent]
            if *continuation_round == round
                && *continuation_task == task_id
                && *continuation_attempt == attempt_id
                && matches!(*role, "primary" | "secondary")
                && *continuation_agent == agent =>
        {
            true
        }
        ["implementation", continuation_round, continuation_task, continuation_attempt, continuation_agent]
            if *continuation_round == round
                && *continuation_task == task_id
                && *continuation_attempt == attempt_id
                && *continuation_agent == agent =>
        {
            false
        }
        _ => bail!("wake-backend-receipt continuation identity is not canonical"),
    };
    let provider = required("providerKind")?;
    if !matches!(provider, "codex" | "opencode" | "smartclaw")
        || required("receiptKind")? != provider
    {
        bail!("wake-backend-receipt provider/receipt kind mismatch");
    }
    for key in [
        "requestMessageSha256",
        "renderedMessageSha256",
        "windowSha256",
    ] {
        full_sha256(required(key)?, key)?;
    }
    if required("backendState")? != "accepted"
        || required("logPath")?.is_empty()
        || required("observedSessionId")?.is_empty()
        || payload
            .get("probeOffset")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        || !payload
            .get("probeEnd")
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|value| value > 0)
    {
        bail!("wake-backend-receipt channel/window fields are invalid");
    }
    let receipt_request_session = payload.get("requestSessionId");
    let request_session_valid = if provider == "smartclaw" {
        receipt_request_session
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
    } else {
        receipt_request_session.is_some_and(serde_json::Value::is_null)
    };
    if !request_session_valid {
        bail!("wake-backend-receipt requestSessionId is invalid");
    }
    if prior.iter().any(|candidate| {
        candidate.kind == "AgentEventReceived"
            && event_payload_str(candidate, "agentEvent") == Some("wake-backend-receipt")
            && event_payload_str(candidate, "wakeId") == Some(wake_id)
    }) {
        bail!("duplicate wake-backend-receipt in committed prefix/suffix");
    }
    let wakes = prior
        .iter()
        .filter(|candidate| {
            candidate.kind == "WakeIssued"
                && candidate.actor == "runtime:orch"
                && candidate.round.as_deref() == Some(round)
                && event_payload_str(candidate, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    if wakes.len() != 1 {
        bail!("wake-backend-receipt requires exactly one preceding WakeIssued");
    }
    let wake = wakes[0];
    let same = wake.task_id.as_deref() == Some(task_id)
        && event_payload_str(wake, "continuationId") == Some(continuation_id)
        && event_payload_str(wake, "attemptId") == Some(attempt_id)
        && event_payload_str(wake, "agent") == Some(agent)
        && event_payload_str(wake, "providerKind") == Some(provider)
        && event_payload_str(wake, "requestMessageSha256")
            == Some(required("requestMessageSha256")?)
        && event_payload_str(wake, "renderedMessageSha256")
            == Some(required("renderedMessageSha256")?)
        && event_payload_str(wake, "logPath") == Some(required("logPath")?)
        && event_payload_str(wake, "backendState") == Some("pending")
        && wake
            .payload
            .as_ref()
            .and_then(|value| value.get("probeOffset"))
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && wake
            .payload
            .as_ref()
            .and_then(|value| value.get("requestSessionId"))
            == receipt_request_session;
    if !same {
        bail!("wake-backend-receipt does not exactly bind its preceding WakeIssued");
    }
    if review_continuation {
        let reviews = prior
            .iter()
            .filter(|candidate| {
                candidate.kind == "ReviewRequested"
                    && candidate.actor == "runtime:orch"
                    && candidate.task_id.as_deref() == Some(task_id)
                    && candidate.round.as_deref() == Some(round)
                    && event_payload_str(candidate, "wakeId") == Some(wake_id)
                    && event_payload_str(candidate, "continuationId") == Some(continuation_id)
                    && event_payload_str(candidate, "attemptId") == Some(attempt_id)
                    && event_payload_str(candidate, "agent") == Some(agent)
                    && event_payload_str(candidate, "requestMessageSha256")
                        == Some(required("requestMessageSha256").unwrap_or_default())
                    && event_payload_str(candidate, "renderedMessageSha256")
                        == Some(required("renderedMessageSha256").unwrap_or_default())
                    && event_payload_str(candidate, "providerKind") == Some(provider)
                    && event_payload_str(candidate, "logPath")
                        == Some(required("logPath").unwrap_or_default())
                    && candidate
                        .payload
                        .as_ref()
                        .and_then(|value| value.get("requestSessionId"))
                        == receipt_request_session
            })
            .count();
        if reviews != 1 {
            bail!("review wake-backend-receipt requires one preceding ReviewRequested");
        }
    }
    Ok(true)
}

fn validate_expected_main_contract(
    root: &Path,
    round: &str,
    task_id: &str,
    main_sha: &str,
    active: &crate::plan::ReadonlyIrValidation,
    ledger_mode: CommittedLedgerMode,
) -> Result<()> {
    let mut modes = fs::read_dir(root.join("coordination/modes"))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect::<Vec<_>>();
    modes.sort();
    let mode = modes
        .first()
        .context("expected-main contract 缺 active mode yaml")?;
    let mode_rel = mode.strip_prefix(root)?.to_string_lossy().to_string();
    let mut sources = vec![
        (
            mode_rel,
            Some(active.candidate.source_bindings.mode_sha256.as_str()),
        ),
        (
            "coordination/PROJECT-BINDING.yaml".to_string(),
            Some(active.candidate.source_bindings.binding_sha256.as_str()),
        ),
    ];
    sources.extend(
        active
            .candidate
            .source_bindings
            .task_cards
            .iter()
            .map(|(path, digest)| (path.clone(), Some(digest.as_str()))),
    );
    sources.extend(
        active
            .candidate
            .source_bindings
            .seed_sources
            .iter()
            .map(|(path, digest)| (path.clone(), Some(digest.as_str()))),
    );
    sources.push((format!("coordination/rounds/{round}/ROUND-IR.yaml"), None));
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    sources.dedup_by(|left, right| left.0 == right.0);
    for (rel, expected_digest) in sources {
        let current = regular_file_bytes(root, &root.join(&rel), "signed source")?;
        let committed = committed_regular_blob_bytes(root, main_sha, &rel, "signed source")?;
        if current != committed {
            bail!("signed source 当前 bytes 未绑定 expected main blob: {rel}");
        }
        if let Some(expected_digest) = expected_digest {
            let (actual, _) = sha_binding(&committed);
            if actual != expected_digest {
                bail!("signed source expected main digest 与 ROUND-IR 不匹配: {rel}");
            }
        }
    }

    let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let current = regular_file_bytes(root, &root.join(&ledger_rel), "round ledger")?;
    let committed = committed_regular_blob_bytes(root, main_sha, &ledger_rel, "round ledger")?;
    match ledger_mode {
        CommittedLedgerMode::Exact if current != committed => {
            bail!("verdict 前 round ledger 必须与 expected main blob exact")
        }
        CommittedLedgerMode::Exact => {}
        CommittedLedgerMode::CanonicalStorageSuffix
        | CommittedLedgerMode::CanonicalVerdictSuffix
        | CommittedLedgerMode::CanonicalRootSuffix
        | CommittedLedgerMode::CanonicalPostMergeSuffix => {
            if !current.starts_with(&committed)
                || (!committed.is_empty()
                    && !committed.ends_with(b"\n")
                    && current.len() > committed.len())
            {
                bail!("merge 前 round ledger 不是 expected main blob 的严格前缀扩展");
            }
            let suffix = &current[committed.len()..];
            let mut prior =
                parse_strict_committed_ledger(&committed, "expected-main committed round ledger")?;
            let suffix_events = suffix
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| {
                    serde_json::from_slice::<EventRecord>(line)
                        .context("merge 前 expected-main ledger suffix 非 canonical JSON event")
                })
                .collect::<Result<Vec<_>>>()?;
            let late = if matches!(
                ledger_mode,
                CommittedLedgerMode::CanonicalRootSuffix
                    | CommittedLedgerMode::CanonicalPostMergeSuffix
            ) {
                let mut complete = prior.clone();
                complete.extend(suffix_events.iter().cloned());
                wake::validate_late_review_deliveries(root, round, &complete)?
            } else {
                wake::ValidatedLateReviewDeliveries::default()
            };
            for (index, event) in suffix_events.iter().enumerate() {
                if !late.event_ids.contains(&event.event_id) {
                    continue;
                }
                let counterpart = match event.kind.as_str() {
                    "ReviewDelivered" => suffix_events.get(index + 1),
                    "EscalationRaised" => index
                        .checked_sub(1)
                        .and_then(|previous| suffix_events.get(previous)),
                    _ => None,
                }
                .context("late review durable pair crosses the expected-main commit boundary")?;
                if !late.event_ids.contains(&counterpart.event_id)
                    || !matches!(
                        (event.kind.as_str(), counterpart.kind.as_str()),
                        ("ReviewDelivered", "EscalationRaised")
                            | ("EscalationRaised", "ReviewDelivered")
                    )
                {
                    bail!("late review durable pair crosses the expected-main commit boundary");
                }
            }
            for event in suffix_events {
                let verdict = event.kind == "VerdictIssued"
                    && event.actor == "verifier:root"
                    && event.task_id.is_some()
                    && event.round.as_deref() == Some(round);
                let merge_started = event.kind == "MergeStarted"
                    && event.actor == "runtime:orch"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round);
                let merge_executed = event.kind == "MergeExecuted"
                    && event.actor == "reviewer:orch-runtime"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round);
                let task_recorded = event.kind == "TaskRecorded"
                    && event.actor == "runtime:orch"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round);
                let postmerge_escalation = event.kind == "EscalationRaised"
                    && event.actor == "reviewer:orch-runtime"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("post-merge-gate");
                let postmerge_other_task_lifecycle = event.round.as_deref() == Some(round)
                    && event.task_id.is_some()
                    && ((event.kind == "MergeStarted" && event.actor == "runtime:orch")
                        || (event.kind == "MergeExecuted"
                            && event.actor == "reviewer:orch-runtime")
                        || (event.kind == "TaskRecorded" && event.actor == "runtime:orch")
                        || (event.kind == "EscalationRaised"
                            && event.actor == "reviewer:orch-runtime"));
                let wake = event.kind == "WakeIssued"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round);
                let backend_receipt = canonical_wake_backend_receipt(&event, &prior, round)?;
                let late_review = late.event_ids.contains(&event.event_id);
                let budget_threshold = canonical_budget_threshold_suffix(&event, round);
                // Storage admission is an inert same-round fact.  It is intentionally
                // task-agnostic here so a concurrent task cannot make an otherwise valid
                // expected-main snapshot stale; Active-barrier append authority remains
                // exact-task in ledger::validate_storage_audit_append.
                let storage = crate::ledger::canonical_gate_storage_audit_event(&event)
                    .is_some_and(|audit| audit.round == round);
                let allowed = storage
                    || (verdict
                        && !matches!(ledger_mode, CommittedLedgerMode::CanonicalStorageSuffix))
                    || (matches!(ledger_mode, CommittedLedgerMode::CanonicalRootSuffix)
                        && (merge_started
                            || wake
                            || backend_receipt
                            || late_review
                            || budget_threshold))
                    || (matches!(ledger_mode, CommittedLedgerMode::CanonicalPostMergeSuffix)
                        && (merge_started
                            || merge_executed
                            || task_recorded
                            || postmerge_escalation
                            || postmerge_other_task_lifecycle
                            || wake
                            || backend_receipt
                            || late_review
                            || post_merge_site_retirement_is_authorized(
                                &event, &prior, round, task_id,
                            )));
                if !allowed {
                    bail!(
                        "merge 前 expected-main ledger suffix 含未授权事件 {}:{}",
                        event.kind,
                        event.actor
                    );
                }
                prior.push(event);
            }
        }
    }
    Ok(())
}

fn required_file_bindings(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    head_sha: &str,
    main_sha: &str,
    implementer_agent: &str,
    ir_task: &crate::plan::IrTask,
    verdict: RootVerdict,
) -> Result<(Vec<ReviewBinding>, Vec<EvidenceBinding>)> {
    let mut reviews = Vec::with_capacity(ir_task.required_reviews.len());
    for required in &ir_task.required_reviews {
        if required.agent == implementer_agent {
            bail!(
                "required reviewer {} 与 current attempt implementer 相同",
                required.agent
            );
        }
        let expectation = ReviewContractExpectation::exact(
            task_id,
            round,
            attempt_id,
            &required.role,
            &required.agent,
            head_sha,
        )?;
        let rel = expectation.artifact_relpath();
        let path = root.join(&rel);
        match fs::symlink_metadata(&path) {
            Ok(_) => ensure_single_review_link(&path, "required review")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("required review stat failed: {}", path.display()))
            }
        }
        let bytes = match regular_file_bytes(root, &path, "required review") {
            Ok(bytes) => bytes,
            Err(canonical_error) => {
                match diagnose_review_site_candidate(root, &expectation) {
                    Ok(Some(diagnostic)) => {
                        bail!("{canonical_error:#}; {diagnostic}")
                    }
                    Ok(None) => return Err(canonical_error),
                    Err(site_error) => {
                        return Err(site_error).context(format!(
                            "required review canonical read failed ({canonical_error:#}); review-site diagnostic failed closed"
                        ))
                    }
                }
            }
        };
        ensure_single_review_link(&path, "required review")?;
        let committed = committed_regular_blob_bytes(root, main_sha, &rel, "required review")?;
        if bytes != committed {
            bail!("required review 当前 bytes 与 expected main blob 不一致: {rel}");
        }
        let checked = check_review_artifact_contract(&bytes, &expectation)
            .with_context(|| format!("review artifact contract 失败: {rel}"))?
            .with_context(|| format!("review artifact 尚未完成: {rel}"))?;
        if checked.substantive_body_len() == 0 {
            bail!("review artifact 缺 substantive body: {rel}");
        }
        report_ignored_review_fields(&rel, checked.ignored_fields());
        if verdict == RootVerdict::Pass && checked.verdict() != "PASS" {
            bail!("root PASS 要求所有 required review PASS: {rel}");
        }
        let (sha256, len) = sha_binding(&committed);
        reviews.push(ReviewBinding {
            path: rel,
            role: required.role.clone(),
            reviewer: required.agent.clone(),
            verdict: checked.verdict().to_string(),
            sha256,
            bytes: len,
        });
    }

    let mut evidence = Vec::new();
    if verdict == RootVerdict::Pass {
        evidence.reserve(ir_task.required_evidence.len());
        for evidence_id in &ir_task.required_evidence {
            let rel =
                format!("coordination/rounds/{round}/evidence/{task_id}-{evidence_id}.json");
            let path = root.join(&rel);
            let bytes = regular_file_bytes(root, &path, "required evidence")?;
            let committed =
                committed_regular_blob_bytes(root, main_sha, &rel, "required evidence")?;
            if bytes != committed {
                bail!("required evidence 当前 bytes 与 expected main blob 不一致: {rel}");
            }
            // Evidence is intentionally opaque to B130, but the `.json` contract
            // must at least be syntactically JSON before its exact bytes are bound.
            let _: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("required evidence 非合法 JSON: {rel}"))?;
            let (sha256, len) = sha_binding(&committed);
            evidence.push(EvidenceBinding {
                id: evidence_id.clone(),
                path: rel,
                sha256,
                bytes: len,
            });
        }
    }
    Ok((reviews, evidence))
}

fn bytes_contain(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && bytes.windows(needle.len()).any(|window| window == needle)
}

fn looks_like_review_frontmatter(bytes: &[u8]) -> bool {
    bytes.starts_with(b"---")
        && [
            b"taskId:".as_slice(),
            b"round:".as_slice(),
            b"attemptId:".as_slice(),
            b"role:".as_slice(),
            b"reviewer:".as_slice(),
            b"verdict:".as_slice(),
            b"reviewedHead:".as_slice(),
        ]
        .iter()
        .all(|needle| bytes_contain(bytes, needle))
}

fn review_tuple_matches(bytes: &[u8], expected: &ReviewContractExpectation) -> Result<bool> {
    let Some((frontmatter, _)) = split_review_frontmatter(bytes)? else {
        return Ok(false);
    };
    let parsed: ReviewFrontmatter =
        serde_yaml::from_str(frontmatter).context("review-site candidate frontmatter malformed")?;
    full_sha(&parsed.reviewed_head, "review-site candidate reviewedHead")?;
    if !matches!(parsed.verdict.as_str(), "PASS" | "FAIL" | "BLOCKED") {
        bail!("review-site candidate verdict 非 PASS|FAIL|BLOCKED");
    }
    Ok(parsed.task_id == expected.task_id
        && parsed.round == expected.round
        && parsed.attempt_id == expected.attempt_id
        && parsed.role == expected.role
        && parsed.reviewer == expected.reviewer
        && parsed.reviewed_head == expected.reviewed_head)
}

/// Read-only recovery diagnostic for a review accidentally committed inside
/// its detached review site. The candidate is never copied or promoted here.
fn diagnose_review_site_candidate(
    root: &Path,
    expected: &ReviewContractExpectation,
) -> Result<Option<String>> {
    let root_text = root
        .to_str()
        .context("repository root is not UTF-8 for review-site diagnostic")?;
    let site = crate::wake::review_site_plan(
        root_text,
        expected.task_id(),
        expected.role(),
        expected.reviewer(),
    )
    .map_err(anyhow::Error::msg)?;
    let site = std::path::PathBuf::from(site.worktree);
    match fs::symlink_metadata(&site) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("stat review-site failed"),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("review-site is not a real directory: {}", site.display())
        }
        Ok(_) => {}
    }
    let canonical_root = fs::canonicalize(root).context("canonicalize repository root failed")?;
    let canonical_site = fs::canonicalize(&site).context("canonicalize review-site failed")?;
    if !canonical_site.starts_with(&canonical_root) {
        bail!("review-site escaped repository root: {}", site.display());
    }
    let gitfile = site.join(".git");
    let gitfile_meta = fs::symlink_metadata(&gitfile).with_context(|| {
        format!(
            "review-site lacks linked-worktree gitfile: {}",
            gitfile.display()
        )
    })?;
    if gitfile_meta.file_type().is_symlink() || !gitfile_meta.is_file() {
        bail!(
            "review-site .git must be a regular non-symlink gitfile: {}",
            gitfile.display()
        );
    }
    let gitfile_bytes = fs::read(&gitfile).context("read review-site .git gitfile failed")?;
    let gitfile_text =
        std::str::from_utf8(&gitfile_bytes).context("review-site .git gitfile is not UTF-8")?;
    let gitfile_line = gitfile_text
        .strip_suffix('\n')
        .filter(|line| !line.is_empty() && !line.contains('\n') && !line.contains('\r'))
        .context("review-site .git gitfile must contain exactly one newline-terminated line")?;
    let declared_git_dir = gitfile_line
        .strip_prefix("gitdir: ")
        .filter(|value| !value.is_empty())
        .context("review-site .git gitfile lacks canonical gitdir prefix")?;
    let declared_git_dir = if Path::new(declared_git_dir).is_absolute() {
        PathBuf::from(declared_git_dir)
    } else {
        site.join(declared_git_dir)
    };
    let canonical_declared_git_dir = fs::canonicalize(&declared_git_dir)
        .context("canonicalize review-site declared gitdir failed")?;
    let git_path = |cwd: &Path, arg: &str, label: &str| -> Result<PathBuf> {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", arg])
            .output()
            .with_context(|| format!("review-site {label} query failed"))?;
        if !output.status.success() {
            bail!(
                "review-site {label} query failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = std::str::from_utf8(&output.stdout)
            .with_context(|| format!("review-site {label} output is not UTF-8"))?
            .strip_suffix('\n')
            .filter(|value| !value.is_empty() && !value.contains('\n') && !value.contains('\r'))
            .with_context(|| format!("review-site {label} output is not one canonical line"))?;
        let path = Path::new(text);
        fs::canonicalize(if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        })
        .with_context(|| format!("canonicalize review-site {label} failed"))
    };
    let resolved_git_dir = git_path(&site, "--git-dir", "git-dir")?;
    if resolved_git_dir != canonical_declared_git_dir {
        bail!("review-site .git gitfile does not bind Git's resolved git-dir");
    }
    let resolved_top = git_path(&site, "--show-toplevel", "toplevel")?;
    if resolved_top != canonical_site {
        bail!(
            "review-site Git toplevel mismatch: expected={} actual={}",
            canonical_site.display(),
            resolved_top.display()
        );
    }
    let root_common = git_path(root, "--git-common-dir", "root common-dir")?;
    let site_common = git_path(&site, "--git-common-dir", "site common-dir")?;
    if root_common != site_common {
        bail!(
            "review-site belongs to another Git common-dir: root={} site={}",
            root_common.display(),
            site_common.display()
        );
    }
    let site_head =
        crate::gitx::rev_parse(&site, "HEAD").context("read review-site current HEAD failed")?;
    full_sha(&site_head, "review-site HEAD")?;
    if !crate::gitx::is_ancestor(&site, expected.reviewed_head(), &site_head)? {
        bail!(
            "review-site HEAD is not descended from fixed reviewedHead: expected={} actual={} site={}",
            expected.reviewed_head(),
            site_head,
            site.display()
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&site)
        .args(["ls-tree", "-r", "--full-tree", "-z", &site_head])
        .output()
        .context("scan review-site HEAD tree failed")?;
    if !output.status.success() {
        bail!(
            "scan review-site HEAD tree failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !output.stdout.is_empty() && output.stdout.last() != Some(&0) {
        bail!("review-site ls-tree output is not NUL terminated");
    }
    let mut matches = Vec::new();
    for raw in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let tab = raw
            .iter()
            .position(|byte| *byte == b'\t')
            .context("review-site ls-tree entry lacks path separator")?;
        let header =
            std::str::from_utf8(&raw[..tab]).context("review-site ls-tree header is not UTF-8")?;
        let path =
            std::str::from_utf8(&raw[tab + 1..]).context("review-site tree path is not UTF-8")?;
        let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || fields[1] != "blob" {
            bail!("review-site ls-tree entry shape is malformed");
        }
        if fields[0] == "120000" && path.ends_with(".md") {
            bail!("review-site Markdown candidate is a symlink in HEAD tree: {path}");
        }
        if !matches!(fields[0], "100644" | "100755") || !path.ends_with(".md") {
            continue;
        }
        let bytes = crate::gitx::show_bytes(&site, &site_head, path)
            .with_context(|| format!("read review-site HEAD blob failed: {path}"))?;
        if !looks_like_review_frontmatter(&bytes) {
            continue;
        }
        if review_tuple_matches(&bytes, expected)? {
            let checked = check_review_artifact_contract(&bytes, expected)
                .with_context(|| format!("review-site candidate contract failed: {path}"))?
                .with_context(|| format!("review-site candidate is incomplete: {path}"))?;
            if checked.substantive_body_len() == 0 {
                bail!("review-site matching candidate lacks substantive body: {path}");
            }
            matches.push(path.to_string());
        }
    }
    let fresh_site_head =
        crate::gitx::rev_parse(&site, "HEAD").context("re-read review-site current HEAD failed")?;
    if fresh_site_head != site_head {
        bail!(
            "review-site HEAD moved during diagnostic: captured={} fresh={}",
            site_head,
            fresh_site_head
        );
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(format!(
            "review-site HEAD={} contains misplaced matching review at {}; 只诊断、不自动搬运",
            site_head, matches[0]
        ))),
        count => bail!(
            "review-site HEAD={} contains {count} matching review candidates; refusing ambiguous recovery",
            site_head
        ),
    }
}

fn latest_collect<'a>(
    events: &'a [EventRecord],
    task_id: &str,
    round: &str,
    ctx: &crate::attempt::DispatchContext,
    head_sha: &str,
    dispatch_position: usize,
) -> Result<(usize, usize, &'a EventRecord)> {
    let (collect_position, event) = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| {
            event.kind == "ReportCollectCompleted"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
        })
        .context("当前 task 缺 ReportCollectCompleted")?;
    if event.actor != "runtime:orch" {
        bail!("ReportCollectCompleted actor 必须为 runtime:orch");
    }
    let payload = event
        .payload
        .as_ref()
        .context("ReportCollectCompleted 缺 payload")?;
    let attempt_id = ctx
        .attempt_id
        .as_deref()
        .context("current dispatch 缺 attemptId")?;
    let attempt_no = ctx.attempt_no.context("current dispatch 缺 attemptNo")?;
    let agent = ctx.agent.as_deref().context("current dispatch 缺 agent")?;
    let base_sha = ctx
        .base_sha
        .as_deref()
        .context("current dispatch 缺 baseSha")?;
    let go_path = ctx
        .go_path
        .as_deref()
        .context("current dispatch 缺 goPath")?;
    let action_id = payload
        .get("actionId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("ReportCollectCompleted 缺非空 actionId")?;
    let receipt_id = payload
        .get("gateReceipt")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("ReportCollectCompleted 缺非空 gateReceipt")?;
    if payload.get("attemptId").and_then(serde_json::Value::as_str) != Some(attempt_id)
        || payload.get("attemptNo").and_then(serde_json::Value::as_u64) != Some(attempt_no as u64)
        || payload.get("agent").and_then(serde_json::Value::as_str) != Some(agent)
        || payload.get("baseSha").and_then(serde_json::Value::as_str) != Some(base_sha)
        || payload.get("goPath").and_then(serde_json::Value::as_str) != Some(go_path)
        || payload.get("branchSha").and_then(serde_json::Value::as_str) != Some(head_sha)
    {
        bail!("latest ReportCollectCompleted 未绑定当前 dispatch/attempt/head lineage");
    }
    let receipts = events
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.event_id == receipt_id)
        .collect::<Vec<_>>();
    if receipts.len() != 1 {
        bail!("ReportCollectCompleted gateReceipt eventId 必须唯一");
    }
    let (receipt_position, receipt) = receipts[0];
    if receipt_position <= dispatch_position
        || collect_position <= dispatch_position
        || receipt_position >= collect_position
        || receipt.kind != "CollectGateSuccessReceipt"
        || receipt.actor != "runtime:orch"
        || receipt.task_id.as_deref() != Some(task_id)
        || receipt.round.as_deref() != Some(round)
    {
        bail!("collect gate receipt envelope/order 不匹配");
    }
    let receipt_payload = receipt
        .payload
        .as_ref()
        .context("CollectGateSuccessReceipt 缺 payload")?;
    for (field, expected) in [
        ("actionId", action_id),
        ("attemptId", attempt_id),
        ("agent", agent),
        ("baseSha", base_sha),
        ("goPath", go_path),
        ("branchSha", head_sha),
    ] {
        if receipt_payload
            .get(field)
            .and_then(serde_json::Value::as_str)
            != Some(expected)
        {
            bail!("CollectGateSuccessReceipt {field} lineage 不匹配");
        }
    }
    if receipt_payload
        .get("attemptNo")
        .and_then(serde_json::Value::as_u64)
        != Some(attempt_no as u64)
    {
        bail!("CollectGateSuccessReceipt attemptNo lineage 不匹配");
    }
    Ok((receipt_position, collect_position, event))
}

#[derive(Debug)]
struct AttemptCollect<'a> {
    attempt_no: usize,
    implementer_agent: String,
    dispatch_position: usize,
    receipt_position: usize,
    collect_position: usize,
    collect: &'a EventRecord,
}

/// Prove the sole legal temporal authorization chains for a root verdict.
/// Normal tasks must be signed before dispatch.  A self-bootstrap task may use
/// one explicitly signed attempt permit, but only to bridge the round's legacy
/// validation marker to its *first* production validation after collect.
fn validate_root_authorization_order(
    events: &[EventRecord],
    round: &str,
    ir_revision: u32,
    validation_digest: &str,
    attempt_id: &str,
    bootstrap_pre_signoff_attempt: Option<&str>,
    lineage: &AttemptCollect<'_>,
    root_position: usize,
) -> Result<()> {
    if root_position > events.len()
        || !(lineage.dispatch_position < lineage.receipt_position
            && lineage.receipt_position < lineage.collect_position
            && lineage.collect_position < root_position)
    {
        bail!("root authorization dispatch/receipt/collect/root order 非 canonical");
    }

    // Historical validation/sign-off proof is prefix-scoped: later serialized
    // tasks must not rewrite the authorization facts of an already-issued root
    // verdict.
    let prefix = &events[..root_position];
    let mut production = Vec::new();
    let mut legacy = Vec::new();
    let mut validation_high_water: Option<crate::plan::TaskValidatedPayload> = None;
    let mut seen_production = false;
    for (position, event) in prefix.iter().enumerate().filter(|(_, event)| {
        event.kind == "TaskValidated"
            && event.actor == "runtime:orch"
            && event.task_id.is_none()
            && event.round.as_deref() == Some(round)
    }) {
        let payload = crate::plan::decode_task_validated(event)?;
        if let Some(previous) = &validation_high_water {
            if payload.ir_revision < previous.ir_revision {
                bail!("root authorization TaskValidated revision 非单调递增");
            }
            if payload.ir_revision == previous.ir_revision
                && payload.validation_digest != previous.validation_digest
            {
                bail!("root authorization 同 revision TaskValidated digest 冲突");
            }
        }
        validation_high_water = Some(payload.clone());
        match crate::plan::classify_validation_digest(&payload.validation_digest)? {
            crate::plan::ValidationDigestClass::Production64 => {
                seen_production = true;
                production.push((position, payload));
            }
            crate::plan::ValidationDigestClass::Legacy16 => {
                if seen_production {
                    bail!("root authorization 禁止 production validation 后出现 legacy16");
                }
                legacy.push((position, payload));
            }
        }
    }
    let matching_production = production
        .iter()
        .filter(|(_, payload)| {
            payload.ir_revision == ir_revision && payload.validation_digest == validation_digest
        })
        .collect::<Vec<_>>();
    if matching_production.len() != 1 {
        bail!("root authorization 要求恰好一条 matching production TaskValidated");
    }
    let production_position = matching_production[0].0;
    if production.last().map(|(position, _)| *position) != Some(production_position) {
        bail!("root authorization matching TaskValidated 必须是 pre-root production high-water");
    }
    if !validation_high_water.as_ref().is_some_and(|payload| {
        payload.ir_revision == ir_revision && payload.validation_digest == validation_digest
    }) {
        bail!("root authorization matching production 必须也是 overall validation high-water");
    }

    let signoffs = crate::plan::matching_user_plan_signoff_positions(
        prefix,
        round,
        ir_revision,
        validation_digest,
    )?;
    if signoffs.len() != 1 {
        bail!("root authorization 要求恰好一条 exact canonical user PlanSignedOff");
    }
    let signoff_position = signoffs[0];

    let normal = production_position < signoff_position
        && signoff_position < lineage.dispatch_position
        && lineage.dispatch_position < lineage.receipt_position
        && lineage.receipt_position < lineage.collect_position
        && lineage.collect_position < root_position;
    if normal {
        return Ok(());
    }

    if bootstrap_pre_signoff_attempt != Some(attempt_id) {
        bail!("pre-signoff collect 缺 signed-IR exact attempt permit");
    }
    if legacy.is_empty() {
        bail!("bootstrap authorization 缺 prior canonical-schema legacy TaskValidated");
    }
    if legacy
        .iter()
        .any(|(position, _)| *position >= lineage.dispatch_position)
    {
        bail!("bootstrap legacy TaskValidated 必须全部早于 dispatch");
    }
    let legacy_high_water = legacy
        .iter()
        .map(|(_, payload)| payload.ir_revision)
        .max()
        .context("bootstrap legacy high-water 缺失")?;
    if ir_revision <= legacy_high_water {
        bail!("bootstrap production revision 必须严格大于 legacy high-water");
    }
    if production.first().map(|(position, _)| *position) != Some(production_position) {
        bail!("bootstrap matching TaskValidated 必须是 round first production validation");
    }
    let migration = lineage.dispatch_position < lineage.receipt_position
        && lineage.receipt_position < lineage.collect_position
        && lineage.collect_position < production_position
        && production_position < signoff_position
        && signoff_position < root_position;
    if !migration {
        bail!("bootstrap authorization order 非 canonical A-prime chain");
    }
    Ok(())
}

fn validate_repo_tuple(root: &Path, task_id: &str, head_sha: &str, main_sha: &str) -> Result<()> {
    full_sha(head_sha, "--expected-head")?;
    full_sha(main_sha, "--expected-main")?;
    let actual_main = crate::gitx::rev_parse(root, "main")?;
    if actual_main != main_sha {
        bail!("main HEAD 漂移：expected {main_sha} actual {actual_main}");
    }
    if crate::gitx::current_branch(root)?.as_deref() != Some("main")
        || crate::gitx::rev_parse(root, "HEAD")? != main_sha
    {
        bail!("主工作区必须检出 expected main HEAD");
    }
    let branch = format!("refs/heads/task/{task_id}");
    let actual_branch = crate::gitx::rev_parse(root, &branch)?;
    if actual_branch != head_sha {
        bail!("task branch HEAD 漂移：expected {head_sha} actual {actual_branch}");
    }
    if crate::gitx::is_ancestor(root, head_sha, main_sha)? {
        bail!("task HEAD 已是 main ancestor；拒绝用 root verdict 洗白已合入提交");
    }
    let wt = root.join(".worktrees").join(task_id);
    if !wt.is_dir() {
        bail!("task worktree 不存在: {}", wt.display());
    }
    if crate::gitx::rev_parse(&wt, "HEAD")? != head_sha {
        bail!("task worktree HEAD 与 expected-head 不符");
    }
    if crate::gitx::current_branch(&wt)?.as_deref() != Some(&format!("task/{task_id}")) {
        bail!("task worktree 未检出 canonical task/{task_id} 分支");
    }
    let status = crate::gitx::porcelain_v2(&wt)?;
    if !status.trim().is_empty() {
        bail!("task worktree 非 clean，拒绝 fixed-HEAD verdict");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainAdvanceVerdict {
    Unmoved,
    CoordinationOnly,
    RefusedNotAncestor,
    RefusedCompilationInput(String),
    RefusedBoundArtifact(String),
    RefusedNonCoordinationPath(String),
}

/// Immutable proof that a no-ff merge commit is bound to the signed task head
/// while any movement between the signed main and the merge's real first
/// parent is coordination-only.  Keeping the real first parent in the proof is
/// important: the live merge path additionally binds it to the main ref read
/// immediately before invoking `git merge`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCommitProof {
    pub merge_sha: String,
    pub first_parent_sha: String,
    pub task_head_sha: String,
    pub expected_main_sha: String,
    pub changed_paths: Vec<String>,
}

pub fn classify_main_advance(
    main_sha: &str,
    actual_main: &str,
    main_is_ancestor: bool,
    changed_paths: &[String],
    bound_artifacts: &[String],
) -> MainAdvanceVerdict {
    if main_sha == actual_main {
        return MainAdvanceVerdict::Unmoved;
    }
    if !main_is_ancestor {
        return MainAdvanceVerdict::RefusedNotAncestor;
    }
    if let Some(path) = changed_paths.iter().find(|path| path.starts_with("orch/")) {
        return MainAdvanceVerdict::RefusedCompilationInput(path.clone());
    }
    if let Some(path) = changed_paths
        .iter()
        .find(|path| bound_artifacts.iter().any(|bound| bound == *path))
    {
        return MainAdvanceVerdict::RefusedBoundArtifact(path.clone());
    }
    if let Some(path) = changed_paths
        .iter()
        .find(|path| !path.starts_with("coordination/"))
    {
        return MainAdvanceVerdict::RefusedNonCoordinationPath(path.clone());
    }
    MainAdvanceVerdict::CoordinationOnly
}

fn require_authorized_main_advance(
    main_sha: &str,
    actual_main: &str,
    main_is_ancestor: bool,
    changed_paths: &[String],
    bound_artifacts: &[String],
) -> Result<()> {
    match classify_main_advance(
        main_sha,
        actual_main,
        main_is_ancestor,
        changed_paths,
        bound_artifacts,
    ) {
        MainAdvanceVerdict::Unmoved | MainAdvanceVerdict::CoordinationOnly => Ok(()),
        MainAdvanceVerdict::RefusedNotAncestor => {
            bail!("main HEAD 漂移且 expected main 不是 actual main 的祖先")
        }
        MainAdvanceVerdict::RefusedCompilationInput(path) => {
            bail!("main HEAD 漂移区间改动编译输入，裁决门已失效: {path}")
        }
        MainAdvanceVerdict::RefusedBoundArtifact(path) => {
            bail!("main HEAD 漂移区间改动 root PASS 绑定的审查/证据文件: {path}")
        }
        MainAdvanceVerdict::RefusedNonCoordinationPath(path) => {
            bail!("main HEAD 漂移区间含非 coordination 路径: {path}")
        }
    }
}

/// Validate the one canonical merge shape used by live merge accounting,
/// record recovery, and archived round-close verification.
///
/// The merge must have exactly two ordered parents.  Its second parent is the
/// signed task head.  Its first parent may be newer than the signed main only
/// when the signed main is its ancestor and the complete intervening diff is
/// accepted by [`classify_main_advance`].
pub fn validate_merge_commit_shape(
    root: &Path,
    merge_sha: &str,
    expected_main_sha: &str,
    task_head_sha: &str,
    bound_artifacts: &[String],
) -> Result<MergeCommitProof> {
    full_sha(merge_sha, "merge SHA")?;
    full_sha(expected_main_sha, "expected main SHA")?;
    full_sha(task_head_sha, "task head SHA")?;
    let parents = crate::close::commit_parents(root, merge_sha)?;
    if parents.len() != 2 {
        bail!(
            "merge commit 必须恰好两个 parent，actual={} parents={parents:?}",
            parents.len()
        );
    }
    if parents[1] != task_head_sha {
        bail!(
            "merge commit 第二父未绑定 task head：expected={task_head_sha} actual={}",
            parents[1]
        );
    }
    let first_parent_sha = parents[0].clone();
    let main_is_ancestor = crate::gitx::is_ancestor(root, expected_main_sha, &first_parent_sha)?;
    let changed_paths = if first_parent_sha == expected_main_sha {
        Vec::new()
    } else {
        crate::gitx::diff_names(root, expected_main_sha, &first_parent_sha)?
    };
    require_authorized_main_advance(
        expected_main_sha,
        &first_parent_sha,
        main_is_ancestor,
        &changed_paths,
        bound_artifacts,
    )?;
    Ok(MergeCommitProof {
        merge_sha: merge_sha.to_string(),
        first_parent_sha,
        task_head_sha: task_head_sha.to_string(),
        expected_main_sha: expected_main_sha.to_string(),
        changed_paths,
    })
}

fn validate_merge_repo_tuple(
    root: &Path,
    task_id: &str,
    head_sha: &str,
    main_sha: &str,
    bound_artifacts: &[String],
) -> Result<()> {
    full_sha(head_sha, "--expected-head")?;
    full_sha(main_sha, "--expected-main")?;
    let actual_main = crate::gitx::rev_parse(root, "main")?;
    let main_is_ancestor = crate::gitx::is_ancestor(root, main_sha, &actual_main)?;
    let changed_paths = if actual_main == main_sha {
        Vec::new()
    } else {
        crate::gitx::diff_names(root, main_sha, &actual_main)?
    };
    require_authorized_main_advance(
        main_sha,
        &actual_main,
        main_is_ancestor,
        &changed_paths,
        bound_artifacts,
    )?;
    if crate::gitx::current_branch(root)?.as_deref() != Some("main")
        || crate::gitx::rev_parse(root, "HEAD")? != actual_main
    {
        bail!("主工作区必须检出 actual main HEAD");
    }
    let branch = format!("refs/heads/task/{task_id}");
    let actual_branch = crate::gitx::rev_parse(root, &branch)?;
    if actual_branch != head_sha {
        bail!("task branch HEAD 漂移：expected {head_sha} actual {actual_branch}");
    }
    if crate::gitx::is_ancestor(root, head_sha, &actual_main)? {
        bail!("task HEAD 已是 main ancestor；拒绝用 root verdict 洗白已合入提交");
    }
    let wt = root.join(".worktrees").join(task_id);
    if !wt.is_dir() {
        bail!("task worktree 不存在: {}", wt.display());
    }
    if crate::gitx::rev_parse(&wt, "HEAD")? != head_sha {
        bail!("task worktree HEAD 与 expected-head 不符");
    }
    if crate::gitx::current_branch(&wt)?.as_deref() != Some(&format!("task/{task_id}")) {
        bail!("task worktree 未检出 canonical task/{task_id} 分支");
    }
    let status = crate::gitx::porcelain_v2(&wt)?;
    if !status.trim().is_empty() {
        bail!("task worktree 非 clean，拒绝 fixed-HEAD verdict");
    }
    Ok(())
}

fn current_attempt_and_collect<'a>(
    events: &'a [EventRecord],
    task_id: &str,
    round: &str,
    requested_attempt: &str,
    head_sha: &str,
) -> Result<AttemptCollect<'a>> {
    let ctx = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    if ctx.is_legacy == Some(true) {
        bail!("root verdict 禁止 legacy dispatch identity");
    }
    if ctx.attempt_id.as_deref() != Some(requested_attempt) {
        bail!("--attempt 不是当前 attempt");
    }
    let (dispatch_position, dispatch) = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| {
            event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task_id)
        })
        .context("current attempt 缺 DispatchIssued")?;
    if dispatch.actor != "runtime:orch"
        || dispatch.round.as_deref() != Some(round)
        || dispatch.task_id.as_deref() != Some(task_id)
    {
        bail!("current DispatchIssued envelope 非 canonical runtime tuple");
    }
    let attempt_no = ctx.attempt_no.context("current dispatch 缺 attemptNo")?;
    let implementer = ctx.agent.clone().context("current dispatch 缺 agent")?;
    let (receipt_position, collect_position, collect) =
        latest_collect(events, task_id, round, &ctx, head_sha, dispatch_position)?;
    Ok(AttemptCollect {
        attempt_no,
        implementer_agent: implementer,
        dispatch_position,
        receipt_position,
        collect_position,
        collect,
    })
}

fn archived_attempt_and_collect<'a>(
    events: &'a [EventRecord],
    task_id: &str,
    round: &str,
    requested_attempt: &str,
    head_sha: &str,
) -> Result<AttemptCollect<'a>> {
    let ctx = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    if ctx.is_legacy == Some(true) || ctx.attempt_id.as_deref() != Some(requested_attempt) {
        bail!("archived root PASS 未绑定 final modern dispatch attempt");
    }
    let (dispatch_position, dispatch) = events
        .iter()
        .enumerate()
        .rev()
        .find(|(_, event)| {
            event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task_id)
        })
        .context("archived final attempt 缺 DispatchIssued")?;
    if dispatch.actor != "runtime:orch" || dispatch.round.as_deref() != Some(round) {
        bail!("archived DispatchIssued envelope 非 canonical");
    }
    let attempt_no = ctx.attempt_no.context("archived dispatch 缺 attemptNo")?;
    let implementer = ctx.agent.clone().context("archived dispatch 缺 agent")?;
    let (receipt_position, collect_position, collect) =
        latest_collect(events, task_id, round, &ctx, head_sha, dispatch_position)?;
    Ok(AttemptCollect {
        attempt_no,
        implementer_agent: implementer,
        dispatch_position,
        receipt_position,
        collect_position,
        collect,
    })
}

fn validate_current_implementer(ir: &crate::plan::RoundIr, agent: &str) -> Result<()> {
    if !ir
        .scheduling
        .allowed_agents
        .iter()
        .any(|allowed| allowed == agent)
    {
        bail!("current implementer {agent} 未获 ROUND-IR allowedAgents 授权");
    }
    let capacity = ir
        .scheduling
        .capacities
        .get(agent)
        .with_context(|| format!("current implementer {agent} 缺 ROUND-IR capacity"))?;
    if capacity.agent == 0
        || capacity.quota == 0
        || !capacity.roles.iter().any(|role| role == "implement")
    {
        bail!("current implementer {agent} 缺有效 implement capacity");
    }
    Ok(())
}

fn parse_root_payload(event: &EventRecord) -> Result<RootVerdictPayload> {
    let payload = event
        .payload
        .clone()
        .with_context(|| format!("VerdictIssued {} 缺 payload", event.event_id))?;
    serde_json::from_value(payload)
        .with_context(|| format!("VerdictIssued {} root payload 非 canonical", event.event_id))
}

fn matching_existing_root_verdict(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    core: &RootVerdictPayload,
) -> Result<Option<(usize, RootVerdictPayload)>> {
    let mut root_events = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
    }) {
        let payload = parse_root_payload(event)?;
        if payload.attempt_id == core.attempt_id {
            root_events.push((position, event, payload));
        }
    }
    if root_events.is_empty() {
        return Ok(None);
    }
    if root_events.len() != 1 {
        bail!("task 已有多条 root VerdictIssued，fail closed");
    }
    let (position, _, existing) = root_events.remove(0);
    let same_core = existing.verdict == core.verdict
        && existing.reason == core.reason
        && existing.ir_revision == core.ir_revision
        && existing.validation_digest == core.validation_digest
        && existing.attempt_id == core.attempt_id
        && existing.attempt_no == core.attempt_no
        && existing.implementer_agent == core.implementer_agent
        && existing.head_sha == core.head_sha
        && existing.main_head_sha == core.main_head_sha
        && existing.collect_completed_event_id == core.collect_completed_event_id
        && existing.bootstrap_pre_signoff_attempt == core.bootstrap_pre_signoff_attempt
        && existing.reviews == core.reviews
        && existing.evidence == core.evidence;
    if !same_core {
        bail!("既有 root verdict 与请求 tuple 冲突");
    }
    Ok(Some((position, existing)))
}

fn validate_existing_verdict_gates(
    root: &Path,
    task_id: &str,
    attempt_id: &str,
    ir_task: &crate::plan::IrTask,
    committed_binding: &binding::Binding,
    payload: &RootVerdictPayload,
) -> Result<()> {
    let available = committed_binding
        .commands
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let names =
        binding::resolve_gates(&ir_task.gates_fast, &available).map_err(anyhow::Error::msg)?;
    if names.is_empty() || payload.gates.len() != names.len() {
        bail!("既有 root verdict gates 与 card gate 集合不一致");
    }
    for (name, bound) in names.iter().zip(&payload.gates) {
        if bound.name != *name {
            bail!("既有 root verdict gate 顺序/名称不一致");
        }
        if payload.verdict == "PASS" && bound.exit_code != 0 {
            bail!("既有 root PASS 含红 gate");
        }
        let rel = format!(
            "coordination/runtime/logs/{task_id}-{attempt_id}-root-verdict-gate-{name}.log"
        );
        let bytes = regular_file_bytes(root, &root.join(&rel), "root verdict gate log")?;
        let (sha256, len) = sha_binding(&bytes);
        if sha256 != bound.log_sha256 || len != bound.log_bytes {
            bail!("既有 root verdict gate log bytes 已漂移: {name}");
        }
    }
    Ok(())
}

fn run_verdict_gates(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    ir_task: &crate::plan::IrTask,
    committed_binding: &binding::Binding,
) -> Result<(Vec<gate::GateResult>, Vec<VerdictGateBinding>)> {
    let available = committed_binding
        .commands
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let names =
        binding::resolve_gates(&ir_task.gates_fast, &available).map_err(anyhow::Error::msg)?;
    if names.is_empty() {
        bail!("root verdict gate 集合为空");
    }
    let wt = root.join(".worktrees").join(task_id);
    let log_dir = root.join("coordination/runtime/logs");
    let mut results = Vec::with_capacity(names.len());
    let mut bindings = Vec::with_capacity(names.len());
    for name in names {
        let spec = committed_binding
            .commands
            .get(&name)
            .with_context(|| format!("绑定缺命令: {name}"))?;
        let result = gate::run_gate_with_audit_identity(
            root,
            round,
            ledger::GateAuditIdentity::Attempt {
                task_id,
                attempt_id,
            },
            &name,
            spec,
            &wt,
            &log_dir,
            &format!("{task_id}-{attempt_id}-root-verdict"),
        )?;
        let bytes = fs::read(&result.log_path)
            .with_context(|| format!("读取 verdict gate log 失败: {}", result.log_path))?;
        let (sha256, len) = sha_binding(&bytes);
        bindings.push(VerdictGateBinding {
            name: name.clone(),
            exit_code: result.exit_code,
            log_sha256: sha256,
            log_bytes: len,
        });
        results.push(result);
    }
    Ok((results, bindings))
}

#[derive(Debug)]
struct PendingRootPass {
    task_id: String,
    attempt_id: String,
    attempt_no: usize,
    implementer_agent: String,
    head_sha: String,
    main_head_sha: String,
    collect_completed_event_id: String,
    verdict_event_id: String,
    started: bool,
}

fn validate_pending_merge_closure_terminal(
    event: &EventRecord,
    round: &str,
    active: &PendingRootPass,
) -> Result<()> {
    let payload = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("merge barrier closure lacks adjacent AttemptBlocked object")?;
    const KEYS: &[&str] = &["attemptId", "attemptNo", "agent", "stage", "reason"];
    if event.kind != "AttemptBlocked"
        || event.actor != "runtime:orch"
        || event.task_id.as_deref() != Some(active.task_id.as_str())
        || event.round.as_deref() != Some(round)
        || payload.len() != KEYS.len()
        || KEYS.iter().any(|key| !payload.contains_key(*key))
        || payload.get("attemptId").and_then(serde_json::Value::as_str)
            != Some(active.attempt_id.as_str())
        || payload.get("attemptNo").and_then(serde_json::Value::as_u64)
            != Some(active.attempt_no as u64)
        || payload.get("agent").and_then(serde_json::Value::as_str)
            != Some(active.implementer_agent.as_str())
        || payload.get("stage").and_then(serde_json::Value::as_str) != Some("merge-conflict")
        || payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|reason| reason.trim().is_empty())
    {
        bail!("merge barrier closure lacks adjacent exact AttemptBlocked");
    }
    Ok(())
}

/// Fold the current round's root-PASS lifecycle before creating another
/// verdict.  The main hook can only authorize one concrete merge tuple, so a
/// second unresolved PASS would otherwise create a permanent two-owner
/// barrier.  Exact replay of the same task/attempt remains legal.
fn ensure_single_pending_root_pass(
    events: &[EventRecord],
    round: &str,
    requested_task: &str,
    requested_attempt: &str,
    requested_verdict: RootVerdict,
) -> Result<()> {
    let mut event_ids = BTreeSet::new();
    for event in events {
        if event.event_id.is_empty() || !event_ids.insert(event.event_id.as_str()) {
            bail!("root PASS barrier requires unique non-empty eventId values");
        }
        if matches!(
            event.kind.as_str(),
            "VerdictIssued" | "MergeStarted" | "MergeExecuted"
        ) && event.round.as_deref() != Some(round)
        {
            bail!(
                "root PASS lifecycle event {} lies outside current round {round}",
                event.kind
            );
        }
    }
    let mut pending: Option<PendingRootPass> = None;
    let mut awaiting_closure_terminal = false;
    for event in events {
        if awaiting_closure_terminal {
            let active = pending
                .as_ref()
                .context("merge barrier closure lost its pending root PASS")?;
            validate_pending_merge_closure_terminal(event, round, active)?;
            awaiting_closure_terminal = false;
            pending = None;
            continue;
        }
        if event.round.as_deref() != Some(round) {
            continue;
        }
        match event.kind.as_str() {
            "VerdictIssued" if event.actor == "verifier:root" => {
                let payload = parse_root_payload(event)?;
                if payload.verdict != "PASS" {
                    continue;
                }
                if pending.is_some() {
                    bail!("ledger already contains multiple unresolved root PASS verdicts");
                }
                pending = Some(PendingRootPass {
                    task_id: event
                        .task_id
                        .clone()
                        .filter(|task| !task.is_empty())
                        .context("root PASS lacks taskId")?,
                    attempt_id: payload.attempt_id,
                    attempt_no: payload.attempt_no,
                    implementer_agent: payload.implementer_agent,
                    head_sha: payload.head_sha,
                    main_head_sha: payload.main_head_sha,
                    collect_completed_event_id: payload.collect_completed_event_id,
                    verdict_event_id: event.event_id.clone(),
                    started: false,
                });
            }
            "MergeStarted" => {
                let started: MergeStartedPayload = serde_json::from_value(
                    event
                        .payload
                        .clone()
                        .context("MergeStarted lacks payload")?,
                )
                .context("MergeStarted payload is not canonical")?;
                let active = pending
                    .as_mut()
                    .context("MergeStarted has no preceding unresolved root PASS")?;
                if event.actor != "runtime:orch"
                    || event.task_id.as_deref() != Some(active.task_id.as_str())
                    || active.started
                    || started.attempt_id != active.attempt_id
                    || started.attempt_no != active.attempt_no
                    || started.head_sha != active.head_sha
                    || started.main_head_sha != active.main_head_sha
                    || started.collect_completed_event_id != active.collect_completed_event_id
                    || started.verdict_event_id != active.verdict_event_id
                {
                    bail!("MergeStarted does not exactly bind the pending root PASS");
                }
                active.started = true;
            }
            "MergeExecuted" => {
                let merged: MergeExecutedPayload = serde_json::from_value(
                    event
                        .payload
                        .clone()
                        .context("MergeExecuted lacks payload")?,
                )
                .context("MergeExecuted payload is not canonical")?;
                let active = pending
                    .as_ref()
                    .context("MergeExecuted has no pending root PASS")?;
                if event.actor != "reviewer:orch-runtime"
                    || event.task_id.as_deref() != Some(active.task_id.as_str())
                    || !active.started
                    || merged.policy != "no-ff"
                {
                    bail!("MergeExecuted does not canonically close the pending root PASS");
                }
                full_sha(&merged.merge_sha, "MergeExecuted.mergeSha")?;
                pending = None;
            }
            "AttemptBlocked"
                if event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("stage"))
                    .and_then(serde_json::Value::as_str)
                    == Some("approved-reattempt") =>
            {
                let active = pending
                    .as_ref()
                    .context("approved-reattempt terminal has no pending root PASS")?;
                let payload = event
                    .payload
                    .as_ref()
                    .and_then(serde_json::Value::as_object)
                    .context("approved-reattempt terminal payload is not an object")?;
                let keys = [
                    "attemptId",
                    "attemptNo",
                    "agent",
                    "stage",
                    "verdictEventId",
                    "reason",
                ];
                if payload.len() != keys.len()
                    || keys.iter().any(|key| !payload.contains_key(*key))
                    || event.actor != "runtime:orch"
                    || event.task_id.as_deref() != Some(active.task_id.as_str())
                    || active.started
                    || payload.get("attemptId").and_then(serde_json::Value::as_str)
                        != Some(active.attempt_id.as_str())
                    || payload.get("attemptNo").and_then(serde_json::Value::as_u64)
                        != Some(active.attempt_no as u64)
                    || payload.get("agent").and_then(serde_json::Value::as_str)
                        != Some(active.implementer_agent.as_str())
                    || payload
                        .get("verdictEventId")
                        .and_then(serde_json::Value::as_str)
                        != Some(active.verdict_event_id.as_str())
                    || payload
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(|value| value.trim().is_empty())
                {
                    bail!("approved-reattempt terminal does not canonically close root PASS");
                }
                pending = None;
            }
            "EscalationRaised" => {
                let stage = event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("stage"))
                    .and_then(serde_json::Value::as_str);
                if !matches!(stage, Some("merge-conflict" | "barrier-recovered")) {
                    continue;
                }
                let active = pending
                    .as_ref()
                    .context("barrier closure has no pending root PASS")?;
                if event.actor != "reviewer:orch-runtime"
                    || event.task_id.as_deref() != Some(active.task_id.as_str())
                    || !active.started
                {
                    bail!("barrier closure does not bind the pending root PASS");
                }
                match stage {
                    Some("merge-conflict") => {
                        let payload = event.payload.as_ref().unwrap();
                        if payload.as_object().is_none_or(|object| object.len() != 3)
                            || payload.get("mergeSha") != Some(&serde_json::Value::Null)
                            || payload
                                .get("conflictFiles")
                                .and_then(serde_json::Value::as_array)
                                .is_none_or(|files| {
                                    files.iter().any(|file| {
                                        file.as_str().is_none_or(|path| path.is_empty())
                                    })
                                })
                        {
                            bail!("merge-conflict closure payload is not canonical");
                        }
                    }
                    Some("barrier-recovered") => {
                        if event
                            .payload
                            .as_ref()
                            .and_then(serde_json::Value::as_object)
                            .is_none_or(|object| {
                                object.len() != 1
                                    || object.get("stage").and_then(serde_json::Value::as_str)
                                        != Some("barrier-recovered")
                            })
                        {
                            bail!("barrier-recovered closure payload is not canonical");
                        }
                    }
                    _ => unreachable!(),
                }
                awaiting_closure_terminal = true;
            }
            _ => {}
        }
    }
    if awaiting_closure_terminal {
        bail!("merge barrier closure lacks adjacent exact AttemptBlocked");
    }
    if let Some(active) = pending {
        if requested_verdict != RootVerdict::Pass
            || active.task_id != requested_task
            || active.attempt_id != requested_attempt
        {
            bail!(
                "pending root PASS blocks new verdict: task={} attempt={} verdictEvent={}",
                active.task_id,
                active.attempt_id,
                active.verdict_event_id
            );
        }
    }
    Ok(())
}

pub fn run_root_verdict(
    root: &Path,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
    expected_main: &str,
    verdict: RootVerdict,
    reason: Option<&str>,
    dry_run: bool,
) -> Result<RootVerdictOutcome> {
    crate::close::with_protocol_transition_named(root, "orch verdict", Some(task_id), || {
        run_root_verdict_locked(
            root,
            task_id,
            attempt_id,
            expected_head,
            expected_main,
            verdict,
            reason,
            dry_run,
        )
    })
}

fn run_root_verdict_locked(
    root: &Path,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
    expected_main: &str,
    verdict: RootVerdict,
    reason: Option<&str>,
    dry_run: bool,
) -> Result<RootVerdictOutcome> {
    if matches!(verdict, RootVerdict::Fail | RootVerdict::Blocked)
        && reason.is_none_or(|text| text.trim().is_empty())
    {
        bail!("FAIL/BLOCKED verdict 强制 --reason");
    }
    if matches!(verdict, RootVerdict::Pass) && reason.is_some() {
        bail!("PASS verdict 不接受 --reason");
    }
    full_sha(expected_head, "--expected-head")?;
    full_sha(expected_main, "--expected-main")?;
    let round = crate::current_round(root)?;
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)?;
    if !lr.bad_lines.is_empty() {
        bail!("root verdict 拒绝坏账本");
    }
    ensure_single_pending_root_pass(&lr.events, &round, task_id, attempt_id, verdict)?;
    let active = crate::plan::require_active_round_ir(root, &round, &lr.events)?;
    if active.candidate.verification.mode != "root-manual-fixed-head" {
        bail!("orch verdict 仅适用于 root-manual-fixed-head");
    }
    validate_expected_main_contract(
        root,
        &round,
        task_id,
        expected_main,
        &active,
        root_verdict_ledger_mode(&lr.events, &round, task_id),
    )?;
    let ir_task = active
        .candidate
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("ROUND-IR 不含 task {task_id}"))?;
    validate_repo_tuple(root, task_id, expected_head, expected_main)?;
    let lineage =
        current_attempt_and_collect(&lr.events, task_id, &round, attempt_id, expected_head)?;
    validate_current_implementer(&active.candidate, &lineage.implementer_agent)?;
    let (reviews, evidence) = required_file_bindings(
        root,
        &round,
        task_id,
        attempt_id,
        expected_head,
        expected_main,
        &lineage.implementer_agent,
        ir_task,
        verdict,
    )?;
    let mut payload = RootVerdictPayload {
        verdict: verdict.as_event_str().to_string(),
        reason: reason.map(str::trim).map(str::to_string),
        ir_revision: active.persisted_revision,
        validation_digest: active.persisted_digest.clone(),
        attempt_id: attempt_id.to_string(),
        attempt_no: lineage.attempt_no,
        implementer_agent: lineage.implementer_agent.clone(),
        head_sha: expected_head.to_string(),
        main_head_sha: expected_main.to_string(),
        collect_completed_event_id: lineage.collect.event_id.clone(),
        bootstrap_pre_signoff_attempt: ir_task.bootstrap_pre_signoff_attempt.clone(),
        reviews,
        evidence,
        gates: Vec::new(),
    };

    let signed_binding = committed_binding(root, expected_main)?;
    let existing = matching_existing_root_verdict(&lr.events, &round, task_id, &payload)?;
    let root_position = existing
        .as_ref()
        .map(|(position, _)| *position)
        .unwrap_or(lr.events.len());
    validate_root_authorization_order(
        &lr.events,
        &round,
        payload.ir_revision,
        &payload.validation_digest,
        attempt_id,
        payload.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        root_position,
    )?;
    if let Some((_, existing)) = existing {
        validate_existing_verdict_gates(
            root,
            task_id,
            attempt_id,
            ir_task,
            &signed_binding,
            &existing,
        )?;
        confirm_replayed_verdict_durable(&ledger_path, &round, task_id, &payload)?;
        return Ok(RootVerdictOutcome {
            appended: false,
            dry_run,
            verdict: payload.verdict,
            gates: Vec::new(),
        });
    }

    let before_main = crate::gitx::rev_parse(root, "main")?;
    let before_task = crate::gitx::rev_parse(root, &format!("refs/heads/task/{task_id}"))?;
    let (gate_results, gate_bindings) =
        run_verdict_gates(root, &round, task_id, attempt_id, ir_task, &signed_binding)?;
    payload.gates = gate_bindings;
    if verdict == RootVerdict::Pass {
        if let Some(red) = gate_results.iter().find(|gate| gate.exit_code != 0) {
            bail!("root PASS gate {} 红（exit {}）", red.name, red.exit_code);
        }
    }
    validate_repo_tuple(root, task_id, expected_head, expected_main)?;
    if crate::gitx::rev_parse(root, "main")? != before_main
        || crate::gitx::rev_parse(root, &format!("refs/heads/task/{task_id}"))? != before_task
    {
        bail!("verdict gates 前后 task/main HEAD 发生移动");
    }

    if dry_run {
        return Ok(RootVerdictOutcome {
            appended: false,
            dry_run: true,
            verdict: payload.verdict,
            gates: gate_results,
        });
    }

    let payload_value = serde_json::to_value(&payload)?;
    let appended = ledger::append_checked(root, &round, |events| {
        ensure_single_pending_root_pass(events, &round, task_id, attempt_id, verdict)?;
        let fresh_active = crate::plan::require_active_round_ir(root, &round, events)?;
        if fresh_active.persisted_revision != payload.ir_revision
            || fresh_active.persisted_digest != payload.validation_digest
        {
            bail!("verdict append 前 active IR revision/digest 漂移");
        }
        validate_expected_main_contract(
            root,
            &round,
            task_id,
            expected_main,
            &fresh_active,
            root_verdict_ledger_mode(events, &round, task_id),
        )?;
        validate_repo_tuple(root, task_id, expected_head, expected_main)?;
        let fresh_lineage =
            current_attempt_and_collect(events, task_id, &round, attempt_id, expected_head)?;
        validate_current_implementer(&fresh_active.candidate, &fresh_lineage.implementer_agent)?;
        if fresh_lineage.attempt_no != payload.attempt_no
            || fresh_lineage.implementer_agent != payload.implementer_agent
            || fresh_lineage.collect.event_id != payload.collect_completed_event_id
        {
            bail!("verdict append 前 attempt/collect 漂移");
        }
        let fresh_task = fresh_active
            .candidate
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .context("append 前 ROUND-IR task 消失")?;
        if fresh_task.bootstrap_pre_signoff_attempt != payload.bootstrap_pre_signoff_attempt {
            bail!("verdict append 前 bootstrapPreSignoffAttempt 漂移");
        }
        let (fresh_reviews, fresh_evidence) = required_file_bindings(
            root,
            &round,
            task_id,
            attempt_id,
            expected_head,
            expected_main,
            &fresh_lineage.implementer_agent,
            fresh_task,
            verdict,
        )?;
        if fresh_reviews != payload.reviews || fresh_evidence != payload.evidence {
            bail!("verdict append 前 review/evidence bytes 漂移");
        }
        let existing = matching_existing_root_verdict(events, &round, task_id, &payload)?;
        let root_position = existing
            .as_ref()
            .map(|(position, _)| *position)
            .unwrap_or(events.len());
        validate_root_authorization_order(
            events,
            &round,
            payload.ir_revision,
            &payload.validation_digest,
            attempt_id,
            payload.bootstrap_pre_signoff_attempt.as_deref(),
            &fresh_lineage,
            root_position,
        )?;
        if let Some((_, existing)) = existing {
            validate_existing_verdict_gates(
                root,
                task_id,
                attempt_id,
                fresh_task,
                &signed_binding,
                &existing,
            )?;
            return Ok(Vec::new());
        }
        Ok(vec![ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some(task_id),
            Some(&round),
            payload_value.clone(),
        )])
    })?;
    if appended == 0 {
        // H21 机械侧根治：幂等重放（decide 在锁内 fresh-read 里找到既有
        // verdict 而追加 0 条）绝不能只凭内存快照报成功——回读账本确认该
        // attempt 的 canonical VerdictIssued 真实在场，否则 Err。
        confirm_replayed_verdict_durable(&ledger_path, &round, task_id, &payload)?;
    }
    Ok(RootVerdictOutcome {
        appended: appended == 1,
        dry_run: false,
        verdict: payload.verdict,
        gates: gate_results,
    })
}

/// H21：幂等重放路径在回报成功前的机械复核——重新打开账本文件 fresh-read，
/// 确认该 attempt 的 canonical root VerdictIssued 真实在场；坏行/缺失一律
/// Err，绝不按成功上报（planner 侧 seal.sh 兜底之外的机械侧根治）。
fn confirm_replayed_verdict_durable(
    ledger_path: &Path,
    round: &str,
    task_id: &str,
    payload: &RootVerdictPayload,
) -> Result<()> {
    let relr = read_ledger(ledger_path).context("幂等重放复核回读账本失败")?;
    if !relr.bad_lines.is_empty() {
        bail!("幂等重放复核发现账本坏行，拒绝按成功上报");
    }
    if matching_existing_root_verdict(&relr.events, round, task_id, payload)?.is_none() {
        bail!(
            "幂等重放（appended=0）但回读账本无 attempt {} 的 canonical root VerdictIssued，拒绝按成功上报",
            payload.attempt_id
        );
    }
    Ok(())
}

/// Revalidate the exact root PASS barrier immediately before merge.  No event
/// field is trusted without recomputing current attempt/collect/refs and the
/// bytes of every review/evidence artifact.
pub fn validate_root_merge_authorization(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<RootMergeAuthorization> {
    let active = crate::plan::require_active_round_ir(root, round, events)?;
    if active.candidate.verification.mode != "root-manual-fixed-head" {
        bail!("root merge authorization 仅适用于 root-manual-fixed-head");
    }
    let ir_task = active
        .candidate
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("ROUND-IR 不含 task {task_id}"))?;
    let current_ctx = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    let current_attempt = current_ctx
        .attempt_id
        .as_deref()
        .context("merge authorization 缺 current attempt")?;
    let mut root_events = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "VerdictIssued"
            && event.task_id.as_deref() == Some(task_id)
            && event.actor == "verifier:root"
            && event.round.as_deref() == Some(round)
    }) {
        let payload = parse_root_payload(event)?;
        if payload.attempt_id == current_attempt {
            root_events.push((position, event, payload));
        }
    }
    if root_events.len() != 1 {
        bail!("merge 要求恰好一条 verifier:root VerdictIssued");
    }
    let (root_position, event, payload) = root_events.remove(0);
    // H99 后续（r62 收轮非门审查 P1-1/P1-2）：root PASS 之后出现的 attempt 终态事件
    // 使这个 verdict 作废——attempt 已死，它的授权不能再驱动一次合并。此前无人执行
    // 这条不变量：`current_attempt` 只数 `DispatchIssued`（attempt.rs），终态不退役
    // current attempt；`validate_root_authorization_order` 只扫 root 之前的前缀。
    // 缺口的后果不是「多合并一次」而是**楔死**：终态之后落的 `MergeStarted` 会被
    // `close::merge_started_is_stale` 判为陈旧 ⇒ 屏障已 ACTIVE 却查无此条 ⇒ 全轮无出口。
    // 在这里 fail-closed 拒绝授权，让它在**落屏障之前**就停下。
    if let Some(terminal) = events[root_position + 1..].iter().find(|candidate| {
        candidate.task_id.as_deref() == Some(task_id)
            && candidate.round.as_deref() == Some(round)
            && crate::attempt::event_terminates_attempt(candidate, current_attempt)
    }) {
        bail!(
            "current attempt {current_attempt} 已被 {} 终结于 root PASS 之后，该 verdict 随 attempt 作废；请铸新 attempt",
            terminal.kind
        );
    }
    validate_expected_main_contract(
        root,
        round,
        task_id,
        &payload.main_head_sha,
        &active,
        CommittedLedgerMode::CanonicalRootSuffix,
    )?;
    if payload.ir_revision != active.persisted_revision
        || payload.validation_digest != active.persisted_digest
    {
        bail!("root PASS 未绑定 current active IR revision/digest");
    }
    if payload.verdict != "PASS" || payload.gates.is_empty() {
        bail!("merge 要求 root PASS 且 gates 非空");
    }
    if payload.gates.iter().any(|gate| gate.exit_code != 0) {
        bail!("root PASS payload 含红 gate");
    }
    let bound_artifacts = payload
        .reviews
        .iter()
        .map(|binding| binding.path.clone())
        .chain(payload.evidence.iter().map(|binding| binding.path.clone()))
        .collect::<Vec<_>>();
    validate_merge_repo_tuple(
        root,
        task_id,
        &payload.head_sha,
        &payload.main_head_sha,
        &bound_artifacts,
    )?;
    if payload.bootstrap_pre_signoff_attempt != ir_task.bootstrap_pre_signoff_attempt {
        bail!("root PASS bootstrapPreSignoffAttempt 未绑定 current active IR");
    }
    let lineage = current_attempt_and_collect(
        events,
        task_id,
        round,
        &payload.attempt_id,
        &payload.head_sha,
    )?;
    validate_root_authorization_order(
        events,
        round,
        payload.ir_revision,
        &payload.validation_digest,
        &payload.attempt_id,
        payload.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        root_position,
    )?;
    validate_current_implementer(&active.candidate, &lineage.implementer_agent)?;
    if lineage.attempt_no != payload.attempt_no
        || lineage.implementer_agent != payload.implementer_agent
        || lineage.collect.event_id != payload.collect_completed_event_id
    {
        bail!("root PASS 与 current attempt/latest collect 不一致");
    }
    let (reviews, evidence) = required_file_bindings(
        root,
        round,
        task_id,
        &payload.attempt_id,
        &payload.head_sha,
        &payload.main_head_sha,
        &lineage.implementer_agent,
        ir_task,
        RootVerdict::Pass,
    )?;
    if reviews != payload.reviews || evidence != payload.evidence {
        bail!("root PASS 后 review/evidence bytes 已改变");
    }
    let signed_binding = committed_binding(root, &payload.main_head_sha)?;
    validate_existing_verdict_gates(
        root,
        task_id,
        &payload.attempt_id,
        ir_task,
        &signed_binding,
        &payload,
    )?;
    Ok(RootMergeAuthorization {
        verdict_event_id: event.event_id.clone(),
        attempt_id: payload.attempt_id,
        attempt_no: payload.attempt_no,
        implementer_agent: payload.implementer_agent,
        head_sha: payload.head_sha,
        main_head_sha: payload.main_head_sha,
        collect_completed_event_id: payload.collect_completed_event_id,
        bound_artifacts,
    })
}

/// Revalidate the complete post-merge recovery chain.  This is deliberately
/// separate from merge authorization: after a valid merge the task head is an
/// ancestor of main, so the pre-merge repository predicate no longer applies.
pub fn validate_root_record_authorization(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<RootRecordAuthorization> {
    validate_root_record_authorization_inner(
        root,
        round,
        task_id,
        events,
        RecordAuthorizationMode::MergeExecuted,
    )
}

/// H48 recovery-only authorization.  It proves the same immutable root PASS,
/// dispatch/collect, review/evidence, gate, and MergeStarted chain as ordinary
/// record authorization, but derives the real merge SHA from one canonical
/// `merge-boundary-shape` fact when the interrupted merge never recorded a
/// `MergeExecuted`.  No event is appended by this validator.
pub fn validate_root_boundary_recovery_authorization(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<RootRecordAuthorization> {
    validate_root_record_authorization_inner(
        root,
        round,
        task_id,
        events,
        RecordAuthorizationMode::BoundaryRecovery,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordAuthorizationMode {
    MergeExecuted,
    BoundaryRecovery,
}

fn canonical_event_extra(event: &EventRecord) -> bool {
    event.extra.keys().all(|key| {
        matches!(
            key.as_str(),
            "plannerWakeId" | "initiatorKind" | "invocationMode"
        )
    })
}

fn validate_root_record_authorization_inner(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
    mode: RecordAuthorizationMode,
) -> Result<RootRecordAuthorization> {
    let active = crate::plan::require_active_round_ir(root, round, events)?;
    if active.candidate.verification.mode != "root-manual-fixed-head" {
        bail!("root record authorization 仅适用于 root-manual-fixed-head");
    }
    let ir_task = active
        .candidate
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("ROUND-IR 不含 task {task_id}"))?;
    let current = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    let current_attempt = current
        .attempt_id
        .as_deref()
        .context("record authorization 缺 current attempt")?;

    let mut roots = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "VerdictIssued"
            && event.actor == "verifier:root"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
    }) {
        let payload = parse_root_payload(event)?;
        if payload.attempt_id == current_attempt {
            roots.push((position, event, payload));
        }
    }
    if roots.len() != 1 {
        bail!("record 要求 current attempt 恰好一条 verifier:root VerdictIssued");
    }
    let (root_position, root_event, payload) = roots.remove(0);
    if payload.verdict != "PASS"
        || payload.gates.is_empty()
        || payload.gates.iter().any(|gate| gate.exit_code != 0)
    {
        bail!("record 要求 current attempt canonical root PASS 且 gates 全绿非空");
    }
    if payload.ir_revision != active.persisted_revision
        || payload.validation_digest != active.persisted_digest
    {
        bail!("record root PASS 未绑定 current active IR revision/digest");
    }
    validate_expected_main_contract(
        root,
        round,
        task_id,
        &payload.main_head_sha,
        &active,
        CommittedLedgerMode::CanonicalPostMergeSuffix,
    )?;

    if payload.bootstrap_pre_signoff_attempt != ir_task.bootstrap_pre_signoff_attempt {
        bail!("record root PASS bootstrapPreSignoffAttempt 未绑定 current active IR");
    }
    let lineage = current_attempt_and_collect(
        events,
        task_id,
        round,
        &payload.attempt_id,
        &payload.head_sha,
    )?;
    validate_root_authorization_order(
        events,
        round,
        payload.ir_revision,
        &payload.validation_digest,
        &payload.attempt_id,
        payload.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        root_position,
    )?;
    validate_current_implementer(&active.candidate, &lineage.implementer_agent)?;
    if lineage.attempt_no != payload.attempt_no
        || lineage.implementer_agent != payload.implementer_agent
        || lineage.collect.event_id != payload.collect_completed_event_id
    {
        bail!("record root PASS 与 current attempt/latest collect 不一致");
    }
    let (reviews, evidence) = required_file_bindings(
        root,
        round,
        task_id,
        &payload.attempt_id,
        &payload.head_sha,
        &payload.main_head_sha,
        &lineage.implementer_agent,
        ir_task,
        RootVerdict::Pass,
    )?;
    if reviews != payload.reviews || evidence != payload.evidence {
        bail!("record 前 review/evidence bytes 已改变");
    }
    let signed_binding = committed_binding(root, &payload.main_head_sha)?;
    validate_existing_verdict_gates(
        root,
        task_id,
        &payload.attempt_id,
        ir_task,
        &signed_binding,
        &payload,
    )?;

    let mut starts = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "MergeStarted" && event.task_id.as_deref() == Some(task_id)
    }) {
        if event.actor != "runtime:orch" || event.round.as_deref() != Some(round) {
            bail!("MergeStarted envelope 非 canonical runtime tuple");
        }
        let started: MergeStartedPayload =
            serde_json::from_value(event.payload.clone().context("MergeStarted 缺 payload")?)
                .context("MergeStarted payload 非 canonical")?;
        if started.attempt_id == payload.attempt_id {
            starts.push((position, started));
        }
    }
    if starts.len() != 1 {
        bail!("record 要求 current attempt 恰好一条 canonical MergeStarted");
    }
    let (started_position, started) = starts.remove(0);
    if started_position <= root_position
        || started.attempt_no != payload.attempt_no
        || started.head_sha != payload.head_sha
        || started.main_head_sha != payload.main_head_sha
        || started.collect_completed_event_id != payload.collect_completed_event_id
        || started.verdict_event_id != root_event.event_id
    {
        bail!("MergeStarted 未精确绑定 current root PASS tuple/order");
    }

    let mut merges = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "MergeExecuted" && event.task_id.as_deref() == Some(task_id)
    }) {
        if event.actor != "reviewer:orch-runtime" || event.round.as_deref() != Some(round) {
            bail!("MergeExecuted envelope 非 canonical reviewer tuple");
        }
        let merged: MergeExecutedPayload =
            serde_json::from_value(event.payload.clone().context("MergeExecuted 缺 payload")?)
                .context("MergeExecuted payload 非 canonical")?;
        merges.push((position, merged));
    }
    let (merge_position, merge_sha) = match mode {
        RecordAuthorizationMode::MergeExecuted => {
            if merges.len() != 1 {
                bail!("record 要求恰好一条 canonical MergeExecuted");
            }
            let (merge_position, merged) = merges.remove(0);
            full_sha(&merged.merge_sha, "MergeExecuted.mergeSha")?;
            if merge_position <= started_position || merged.policy != "no-ff" {
                bail!("MergeExecuted policy/order 未绑定 MergeStarted");
            }
            (merge_position, merged.merge_sha)
        }
        RecordAuthorizationMode::BoundaryRecovery => {
            if !merges.is_empty() {
                bail!("H48 recovery 仅接受尚无 MergeExecuted 的 boundary 状态");
            }
            let mut boundaries = Vec::new();
            for (position, event) in events.iter().enumerate().filter(|(_, event)| {
                event.kind == "EscalationRaised"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round)
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("stage"))
                        .and_then(serde_json::Value::as_str)
                        == Some("merge-boundary-shape")
            }) {
                if event.actor != "reviewer:orch-runtime" || !canonical_event_extra(event) {
                    bail!("H48 recovery boundary envelope 非 canonical reviewer tuple");
                }
                let value = event
                    .payload
                    .clone()
                    .context("H48 recovery boundary 缺 payload")?;
                let object = value
                    .as_object()
                    .context("H48 recovery boundary payload 非 object")?;
                let expected_keys = [
                    "stage",
                    "mergeSha",
                    "actualMain",
                    "actualHead",
                    "reason",
                    "hint",
                ];
                if object.len() != expected_keys.len()
                    || expected_keys.iter().any(|key| !object.contains_key(*key))
                {
                    bail!("H48 recovery boundary payload keys 非 canonical");
                }
                let boundary: MergeBoundaryShapePayload = serde_json::from_value(value)
                    .context("H48 recovery boundary payload 非 canonical")?;
                if boundary.stage != "merge-boundary-shape"
                    || boundary.merge_sha.is_some()
                    || boundary.actual_main != boundary.actual_head
                    || boundary.reason.trim().is_empty()
                    || boundary.hint.trim().is_empty()
                {
                    bail!("H48 recovery boundary 未证明 main/HEAD 同一真实候选 merge");
                }
                full_sha(&boundary.actual_main, "boundary actualMain")?;
                boundaries.push((position, boundary.actual_main));
            }
            if boundaries.len() != 1 {
                bail!("H48 recovery 要求恰好一条 canonical merge-boundary-shape");
            }
            let (boundary_position, merge_sha) = boundaries.remove(0);
            if boundary_position <= started_position {
                bail!("H48 recovery boundary 必须晚于 MergeStarted");
            }
            if events.iter().any(|event| {
                event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(task_id)
            }) {
                bail!("H48 recovery 无 MergeExecuted 却已有 TaskRecorded，拒绝修补坏链");
            }
            (boundary_position, merge_sha)
        }
    };
    let actual_main = crate::gitx::rev_parse(root, "main")?;
    if crate::gitx::current_branch(root)?.as_deref() != Some("main")
        || crate::gitx::rev_parse(root, "HEAD")? != actual_main
    {
        bail!("record 要求主工作区检出 main 且 HEAD==main");
    }
    if !crate::gitx::is_ancestor(root, &merge_sha, &actual_main)? {
        bail!("merge SHA 不是 current main ancestor");
    }
    let bound_artifacts = payload
        .reviews
        .iter()
        .map(|binding| binding.path.clone())
        .chain(payload.evidence.iter().map(|binding| binding.path.clone()))
        .collect::<Vec<_>>();
    validate_merge_commit_shape(
        root,
        &merge_sha,
        &payload.main_head_sha,
        &payload.head_sha,
        &bound_artifacts,
    )?;

    let mut recorded = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(task_id)
    }) {
        if event.actor != "runtime:orch" || event.round.as_deref() != Some(round) {
            bail!("TaskRecorded envelope 非 canonical runtime tuple");
        }
        let recorded_payload: TaskRecordedPayload =
            serde_json::from_value(event.payload.clone().context("TaskRecorded 缺 payload")?)
                .context("TaskRecorded payload 非 canonical")?;
        if recorded_payload.post_merge_gates != "all-green" || position <= merge_position {
            bail!("TaskRecorded payload/order 非 canonical post-merge tuple");
        }
        recorded.push(position);
    }
    if recorded.len() > 1 {
        bail!("已有多条 TaskRecorded，fail closed");
    }

    Ok(RootRecordAuthorization {
        verdict_event_id: root_event.event_id.clone(),
        attempt_id: payload.attempt_id,
        attempt_no: payload.attempt_no,
        implementer_agent: lineage.implementer_agent,
        head_sha: payload.head_sha,
        expected_main_sha: payload.main_head_sha,
        merge_sha,
        ir_revision: active.persisted_revision,
        validation_digest: active.persisted_digest,
        already_recorded: !recorded.is_empty(),
    })
}

/// Prove the immutable, ledger-and-git lineage behind an already recorded
/// task.  Round close deliberately uses this archived validator instead of
/// [`validate_root_record_authorization`]: later tasks may legitimately move
/// main or produce a newer IR revision, and mutable review/evidence files are
/// not an authorization source after the merge.  The historical chain itself
/// must nevertheless remain complete and canonical.
pub fn validate_archived_record_chain(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<()> {
    let mut recorded = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "TaskRecorded" && event.task_id.as_deref() == Some(task_id)
    }) {
        if event.actor != "runtime:orch" || event.round.as_deref() != Some(round) {
            bail!("archived TaskRecorded envelope 非 canonical runtime tuple");
        }
        let payload: TaskRecordedPayload = serde_json::from_value(
            event
                .payload
                .clone()
                .context("archived TaskRecorded 缺 payload")?,
        )
        .context("archived TaskRecorded payload 非 canonical")?;
        if payload.post_merge_gates != "all-green" {
            bail!("archived TaskRecorded 必须绑定 all-green post-merge gates");
        }
        recorded.push(position);
    }
    if recorded.len() != 1 {
        bail!("round close 要求 task {task_id} 恰好一条 canonical TaskRecorded");
    }
    let recorded_position = recorded[0];

    let mut merges = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "MergeExecuted" && event.task_id.as_deref() == Some(task_id)
    }) {
        if event.actor != "reviewer:orch-runtime" || event.round.as_deref() != Some(round) {
            bail!("archived MergeExecuted envelope 非 canonical reviewer tuple");
        }
        let payload: MergeExecutedPayload = serde_json::from_value(
            event
                .payload
                .clone()
                .context("archived MergeExecuted 缺 payload")?,
        )
        .context("archived MergeExecuted payload 非 canonical")?;
        merges.push((position, payload));
    }
    if merges.len() != 1 {
        bail!("round close 要求 task {task_id} 恰好一条 canonical MergeExecuted");
    }
    let (merge_position, merged) = merges.remove(0);
    full_sha(&merged.merge_sha, "archived MergeExecuted.mergeSha")?;
    if merged.policy != "no-ff" || merge_position >= recorded_position {
        bail!("archived MergeExecuted policy/order 未绑定 TaskRecorded");
    }

    let (started_position, started) =
        final_recorded_merge_start(events, round, task_id, merge_position, recorded_position)?;

    // Resolve the one root verdict named by MergeStarted.  Other historical
    // FAIL/BLOCKED attempts are allowed, but this final attempt may have only
    // one root verdict and it must own the referenced event id.
    let mut root_events = Vec::new();
    for (position, event) in events.iter().enumerate().filter(|(_, event)| {
        event.kind == "VerdictIssued" && event.task_id.as_deref() == Some(task_id)
    }) {
        let payload_attempt = event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("attemptId"))
            .and_then(serde_json::Value::as_str);
        if payload_attempt != Some(started.attempt_id.as_str()) {
            continue;
        }
        if event.actor != "verifier:root" || event.round.as_deref() != Some(round) {
            bail!("final attempt VerdictIssued envelope 非 canonical verifier:root tuple");
        }
        let payload = parse_root_payload(event)?;
        root_events.push((position, event, payload));
    }
    if root_events.len() != 1 {
        bail!("round close 要求 final attempt 恰好一条 canonical verifier:root verdict");
    }
    let (root_position, root_event, root_payload) = root_events.remove(0);
    if events
        .iter()
        .filter(|event| event.event_id == started.verdict_event_id)
        .count()
        != 1
        || root_event.event_id != started.verdict_event_id
        || root_position >= started_position
        || root_payload.verdict != "PASS"
        || root_payload.reason.is_some()
        || root_payload.gates.is_empty()
        || root_payload.gates.iter().any(|gate| gate.exit_code != 0)
        || root_payload.attempt_id != started.attempt_id
        || root_payload.attempt_no != started.attempt_no
        || root_payload.head_sha != started.head_sha
        || root_payload.main_head_sha != started.main_head_sha
        || root_payload.collect_completed_event_id != started.collect_completed_event_id
        || !safe_identity_component(&root_payload.implementer_agent)
    {
        bail!("archived root PASS 未精确绑定 MergeStarted tuple/order");
    }
    full_sha(&root_payload.head_sha, "archived root PASS headSha")?;
    full_sha(
        &root_payload.main_head_sha,
        "archived root PASS mainHeadSha",
    )?;
    full_sha256(
        &root_payload.validation_digest,
        "archived root PASS validationDigest",
    )?;
    let historical_ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let historical_ledger_bytes = committed_regular_blob_bytes(
        root,
        &started.main_head_sha,
        &historical_ledger_rel,
        "archived signed ledger",
    )?;
    let historical_events =
        parse_strict_committed_ledger(&historical_ledger_bytes, "archived signed ledger")?;
    if historical_events.len() != root_position
        || !event_values_equal(&historical_events, &events[..root_position])?
    {
        bail!("archived root authorization prefix 未逐事件绑定 mainHead committed ledger");
    }
    let historical_ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let historical_ir_bytes = committed_regular_blob_bytes(
        root,
        &started.main_head_sha,
        &historical_ir_rel,
        "archived signed ROUND-IR",
    )?;
    let historical_ir: crate::plan::RoundIr = serde_yaml::from_slice(&historical_ir_bytes)
        .context("解析 archived mainHead ROUND-IR blob 失败")?;
    if historical_ir.round != round
        || historical_ir.revision != root_payload.ir_revision
        || crate::plan::validation_digest(&historical_ir) != root_payload.validation_digest
    {
        bail!("archived root PASS revision/digest 未绑定 mainHead committed ROUND-IR");
    }
    let historical_task = historical_ir
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("archived committed ROUND-IR 不含 task {task_id}"))?;
    if historical_task.bootstrap_pre_signoff_attempt != root_payload.bootstrap_pre_signoff_attempt {
        bail!("archived root PASS bootstrap permit 未绑定 committed ROUND-IR");
    }
    validate_archived_required_binding_membership(
        &root_payload.reviews,
        &root_payload.evidence,
        &historical_task.required_reviews,
        &historical_task.required_evidence,
    )?;
    if root_payload.reviews.is_empty() || root_payload.evidence.is_empty() {
        bail!("archived root PASS 必须绑定非空 review/evidence 集合");
    }
    let mut review_paths = std::collections::BTreeSet::new();
    let mut review_roles = std::collections::BTreeSet::new();
    let mut reviewers = std::collections::BTreeSet::new();
    let mut primary_count = 0usize;
    for review in &root_payload.reviews {
        if review.role == "primary" {
            primary_count += 1;
        }
        let canonical_path = format!(
            "coordination/rounds/{round}/reviews/{}-{}-{}.md",
            root_payload.attempt_id, review.role, review.reviewer
        );
        if review.path != canonical_path
            || !safe_identity_component(&review.role)
            || !safe_identity_component(&review.reviewer)
            || review.verdict != "PASS"
            || review.reviewer == root_payload.implementer_agent
            || !review_paths.insert(review.path.as_str())
            || !review_roles.insert(review.role.as_str())
            || !reviewers.insert(review.reviewer.as_str())
        {
            bail!("archived root PASS review binding 非 canonical");
        }
        full_sha256(&review.sha256, "archived root PASS review.sha256")?;
    }
    if primary_count != 1 {
        bail!("archived root PASS 必须恰好一个 primary review");
    }
    let mut evidence_ids = std::collections::BTreeSet::new();
    let mut evidence_paths = std::collections::BTreeSet::new();
    for evidence in &root_payload.evidence {
        let canonical_path = format!(
            "coordination/rounds/{round}/evidence/{task_id}-{}.json",
            evidence.id
        );
        if !safe_identity_component(&evidence.id)
            || evidence.path != canonical_path
            || !evidence_ids.insert(evidence.id.as_str())
            || !evidence_paths.insert(evidence.path.as_str())
        {
            bail!("archived root PASS evidence binding 非 canonical");
        }
        full_sha256(&evidence.sha256, "archived root PASS evidence.sha256")?;
    }
    let mut gate_names = std::collections::BTreeSet::new();
    for gate in &root_payload.gates {
        if gate.name.is_empty() || !gate_names.insert(gate.name.as_str()) {
            bail!("archived root PASS gate name 为空");
        }
        full_sha256(&gate.log_sha256, "archived root PASS gate.logSha256")?;
    }

    // Reuse the historical dispatch/receipt/collect decoder.  It performs no
    // mutable artifact reads, but proves the PASS tuple belongs to a modern
    // attempt and to the exact gate receipt/collect lineage.
    let lineage = archived_attempt_and_collect(
        events,
        task_id,
        round,
        &root_payload.attempt_id,
        &root_payload.head_sha,
    )?;
    validate_root_authorization_order(
        events,
        round,
        root_payload.ir_revision,
        &root_payload.validation_digest,
        &root_payload.attempt_id,
        historical_task.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        root_position,
    )?;
    if lineage.attempt_no != root_payload.attempt_no
        || lineage.implementer_agent != root_payload.implementer_agent
        || lineage.collect.event_id != root_payload.collect_completed_event_id
    {
        bail!("archived root PASS 未绑定 exact dispatch/collect lineage/order");
    }

    let current_main = crate::gitx::rev_parse(root, "main")?;
    if !crate::gitx::is_ancestor(root, &merged.merge_sha, &current_main)? {
        bail!("archived MergeExecuted.mergeSha 不是 current main ancestor");
    }
    let bound_artifacts = root_payload
        .reviews
        .iter()
        .map(|binding| binding.path.clone())
        .chain(
            root_payload
                .evidence
                .iter()
                .map(|binding| binding.path.clone()),
        )
        .collect::<Vec<_>>();
    validate_merge_commit_shape(
        root,
        &merged.merge_sha,
        &started.main_head_sha,
        &started.head_sha,
        &bound_artifacts,
    )?;
    Ok(())
}

#[cfg(test)]
mod pending_root_pass_tests {
    use super::*;

    fn root_pass(task: &str, attempt: &str, ordinal: usize) -> EventRecord {
        ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some(task),
            Some("r62"),
            serde_json::to_value(RootVerdictPayload {
                verdict: "PASS".into(),
                reason: None,
                ir_revision: 1,
                validation_digest: "a".repeat(64),
                attempt_id: attempt.into(),
                attempt_no: ordinal,
                implementer_agent: "executor-desktop".into(),
                head_sha: "b".repeat(40),
                main_head_sha: "c".repeat(40),
                collect_completed_event_id: "collect-1".into(),
                bootstrap_pre_signoff_attempt: None,
                reviews: Vec::new(),
                evidence: Vec::new(),
                gates: Vec::new(),
            })
            .unwrap(),
        )
    }

    fn merge_started(task: &str, attempt: &str, ordinal: usize, verdict: &str) -> EventRecord {
        ledger::event(
            "MergeStarted",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "attemptId": attempt,
                "attemptNo": ordinal,
                "headSha": "b".repeat(40),
                "mainHeadSha": "c".repeat(40),
                "collectCompletedEventId": "collect-1",
                "verdictEventId": verdict,
            }),
        )
    }

    #[test]
    fn another_task_cannot_create_a_second_pending_root_pass() {
        let pass = root_pass("B1", "B1-A0001", 1);
        let error = ensure_single_pending_root_pass(
            &[pass.clone()],
            "r62",
            "B2",
            "B2-A0001",
            RootVerdict::Pass,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("pending root PASS blocks"), "{error}");
        ensure_single_pending_root_pass(&[pass], "r62", "B1", "B1-A0001", RootVerdict::Pass)
            .expect("exact root PASS replay remains legal");
    }

    #[test]
    fn exact_approved_reattempt_terminal_releases_the_root_pass() {
        let pass = root_pass("B1", "B1-A0001", 1);
        let verdict_id = pass.event_id.clone();
        let terminal = ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some("B1"),
            Some("r62"),
            serde_json::json!({
                "attemptId": "B1-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "stage": "approved-reattempt",
                "verdictEventId": verdict_id,
                "reason": "operator approved a clean retry",
            }),
        );
        ensure_single_pending_root_pass(
            &[pass, terminal],
            "r62",
            "B2",
            "B2-A0001",
            RootVerdict::Pass,
        )
        .expect("canonical explicit terminal releases the barrier");
    }

    #[test]
    fn canonical_merge_execution_releases_the_root_pass() {
        let pass = root_pass("B1", "B1-A0001", 1);
        let start = merge_started("B1", "B1-A0001", 1, &pass.event_id);
        let merged = ledger::event(
            "MergeExecuted",
            "reviewer:orch-runtime",
            Some("B1"),
            Some("r62"),
            serde_json::json!({"mergeSha": "d".repeat(40), "policy": "no-ff"}),
        );
        ensure_single_pending_root_pass(
            &[pass, start, merged],
            "r62",
            "B2",
            "B2-A0001",
            RootVerdict::Pass,
        )
        .expect("canonical merge closes the pending root PASS");
    }

    fn merge_closure(task: &str, stage: &str) -> EventRecord {
        let payload = if stage == "merge-conflict" {
            serde_json::json!({
                "stage": stage,
                "mergeSha": null,
                "conflictFiles": ["conflict.rs"],
            })
        } else {
            serde_json::json!({"stage": stage})
        };
        ledger::event(
            "EscalationRaised",
            "reviewer:orch-runtime",
            Some(task),
            Some("r62"),
            payload,
        )
    }

    fn merge_terminal(task: &str, attempt: &str, ordinal: usize) -> EventRecord {
        ledger::event(
            "AttemptBlocked",
            "runtime:orch",
            Some(task),
            Some("r62"),
            serde_json::json!({
                "attemptId": attempt,
                "attemptNo": ordinal,
                "agent": "executor-desktop",
                "stage": "merge-conflict",
                "reason": "refs did not move",
            }),
        )
    }

    #[test]
    fn both_no_merge_closures_require_the_immediately_adjacent_exact_terminal() {
        for stage in ["merge-conflict", "barrier-recovered"] {
            let pass = root_pass("B1", "B1-A0001", 1);
            let start = merge_started("B1", "B1-A0001", 1, &pass.event_id);
            let closure = merge_closure("B1", stage);
            let request = |events: &[EventRecord]| {
                ensure_single_pending_root_pass(events, "r62", "B2", "B2-A0001", RootVerdict::Pass)
            };

            let orphan = request(&[pass.clone(), start.clone(), closure.clone()])
                .unwrap_err()
                .to_string();
            assert!(orphan.contains("adjacent exact AttemptBlocked"), "{orphan}");

            let wrong = merge_terminal("B1", "B1-A0002", 2);
            let mismatch = request(&[pass.clone(), start.clone(), closure.clone(), wrong])
                .unwrap_err()
                .to_string();
            assert!(
                mismatch.contains("adjacent exact AttemptBlocked"),
                "{mismatch}"
            );

            let unrelated = ledger::event(
                "BudgetThresholdCrossed",
                "runtime:orch",
                None,
                Some("r62"),
                serde_json::json!({}),
            );
            let nonadjacent = request(&[
                pass.clone(),
                start.clone(),
                closure.clone(),
                unrelated,
                merge_terminal("B1", "B1-A0001", 1),
            ])
            .unwrap_err()
            .to_string();
            assert!(
                nonadjacent.contains("adjacent exact AttemptBlocked"),
                "{nonadjacent}"
            );

            request(&[pass, start, closure, merge_terminal("B1", "B1-A0001", 1)])
                .expect("exact adjacent closure pair releases pending root PASS");
        }
    }

    #[test]
    fn merge_started_and_conflict_closure_bind_the_full_root_tuple() {
        let request = |events: &[EventRecord]| {
            ensure_single_pending_root_pass(events, "r62", "B2", "B2-A0001", RootVerdict::Pass)
        };
        for (field, value) in [
            ("attemptNo", serde_json::json!(2)),
            ("headSha", serde_json::json!("d".repeat(40))),
            ("mainHeadSha", serde_json::json!("e".repeat(40))),
            (
                "collectCompletedEventId",
                serde_json::json!("wrong-collect"),
            ),
        ] {
            let pass = root_pass("B1", "B1-A0001", 1);
            let mut start = merge_started("B1", "B1-A0001", 1, &pass.event_id);
            start.payload.as_mut().unwrap()[field] = value;
            let error = request(&[pass, start]).unwrap_err().to_string();
            assert!(error.contains("exactly bind"), "field={field} {error}");
        }

        let pass = root_pass("B1", "B1-A0001", 1);
        let start = merge_started("B1", "B1-A0001", 1, &pass.event_id);
        let mut closure = merge_closure("B1", "merge-conflict");
        closure.payload.as_mut().unwrap()["conflictFiles"] = serde_json::json!([null]);
        let error = request(&[pass, start, closure, merge_terminal("B1", "B1-A0001", 1)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("payload is not canonical"), "{error}");
    }
}

#[cfg(test)]
mod review_site_diagnostic_tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn b169_misplaced_review_is_diagnosed_from_exact_linked_site_head() {
        let root = crate::util::test_scratch_dir("b204-b169-review-site-diagnostic");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "review diagnostic"]);
        git(
            &root,
            &["config", "user.email", "review-diagnostic@example.invalid"],
        );
        fs::write(root.join("README.md"), "base\n").unwrap();
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["branch", "-M", "main"]);
        let reviewed_head = git(&root, &["rev-parse", "main"]);
        let site = root.join(".worktrees/B169-primary-executor-claw");
        fs::create_dir_all(site.parent().unwrap()).unwrap();
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                site.to_str().unwrap(),
                &reviewed_head,
            ],
        );
        fs::create_dir_all(site.join("misplaced")).unwrap();
        fs::write(
            site.join("misplaced/review.md"),
            format!(
                "---\ntaskId: B169\nround: r62\nattemptId: B169-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: {reviewed_head}\n---\nsubstantive misplaced review\n"
            ),
        )
        .unwrap();
        git(&site, &["add", "misplaced/review.md"]);
        git(&site, &["commit", "-q", "-m", "misplaced review"]);
        let site_head = git(&site, &["rev-parse", "HEAD"]);
        let expectation = ReviewContractExpectation::exact(
            "B169",
            "r62",
            "B169-A0001",
            "primary",
            "executor-claw",
            &reviewed_head,
        )
        .unwrap();
        let diagnostic = diagnose_review_site_candidate(&root, &expectation)
            .unwrap()
            .expect("matching misplaced review must be diagnosed");
        assert!(diagnostic.contains(&site_head), "{diagnostic}");
        assert!(diagnostic.contains("misplaced/review.md"), "{diagnostic}");
        assert!(diagnostic.contains("只诊断、不自动搬运"), "{diagnostic}");

        let fake = root.join(".worktrees/B170-primary-executor-claw");
        fs::create_dir_all(&fake).unwrap();
        let fake_expectation = ReviewContractExpectation::exact(
            "B170",
            "r62",
            "B170-A0001",
            "primary",
            "executor-claw",
            &reviewed_head,
        )
        .unwrap();
        assert!(
            diagnose_review_site_candidate(&root, &fake_expectation).is_err(),
            "ordinary directory must not inherit the parent repository"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod authorization_order_tests {
    use super::*;

    fn validation(revision: u32, digest: &str) -> EventRecord {
        ledger::event(
            "TaskValidated",
            "runtime:orch",
            None,
            Some("r48"),
            crate::plan::task_validated_payload(revision, digest),
        )
    }

    #[test]
    fn later_production_before_root_invalidates_an_older_bootstrap_tuple() {
        let digest_v2 = "a".repeat(64);
        let digest_v3 = "b".repeat(64);
        let events = vec![
            validation(1, "0123456789abcdef"),
            ledger::event(
                "DispatchIssued",
                "runtime:orch",
                Some("B130"),
                Some("r48"),
                serde_json::json!({}),
            ),
            ledger::event(
                "CollectGateSuccessReceipt",
                "runtime:orch",
                Some("B130"),
                Some("r48"),
                serde_json::json!({}),
            ),
            ledger::event(
                "ReportCollectCompleted",
                "runtime:orch",
                Some("B130"),
                Some("r48"),
                serde_json::json!({}),
            ),
            validation(2, &digest_v2),
            ledger::event(
                "PlanSignedOff",
                "user",
                None,
                Some("r48"),
                crate::plan::plan_signed_off_payload("v2", 2, &digest_v2).unwrap(),
            ),
            validation(3, &digest_v3),
        ];
        let lineage = AttemptCollect {
            attempt_no: 1,
            implementer_agent: "executor-desktop".into(),
            dispatch_position: 1,
            receipt_position: 2,
            collect_position: 3,
            collect: &events[3],
        };
        let error = validate_root_authorization_order(
            &events,
            "r48",
            2,
            &digest_v2,
            "B130-A0001",
            Some("B130-A0001"),
            &lineage,
            events.len(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("high-water"), "{error}");
    }

    #[test]
    fn malformed_or_later_legacy_marker_cannot_unlock_bootstrap() {
        let digest = "a".repeat(64);
        for malformed_first in [true, false] {
            let mut events = vec![
                validation(
                    1,
                    if malformed_first {
                        "not-a-real-marker"
                    } else {
                        "0123456789abcdef"
                    },
                ),
                ledger::event(
                    "DispatchIssued",
                    "runtime:orch",
                    Some("B130"),
                    Some("r48"),
                    serde_json::json!({}),
                ),
                ledger::event(
                    "CollectGateSuccessReceipt",
                    "runtime:orch",
                    Some("B130"),
                    Some("r48"),
                    serde_json::json!({}),
                ),
                ledger::event(
                    "ReportCollectCompleted",
                    "runtime:orch",
                    Some("B130"),
                    Some("r48"),
                    serde_json::json!({}),
                ),
                validation(2, &digest),
                ledger::event(
                    "PlanSignedOff",
                    "user",
                    None,
                    Some("r48"),
                    crate::plan::plan_signed_off_payload("v2", 2, &digest).unwrap(),
                ),
            ];
            if !malformed_first {
                events.push(validation(3, "fedcba9876543210"));
            }
            let lineage = AttemptCollect {
                attempt_no: 1,
                implementer_agent: "executor-desktop".into(),
                dispatch_position: 1,
                receipt_position: 2,
                collect_position: 3,
                collect: &events[3],
            };
            let error = validate_root_authorization_order(
                &events,
                "r48",
                2,
                &digest,
                "B130-A0001",
                Some("B130-A0001"),
                &lineage,
                events.len(),
            )
            .unwrap_err()
            .to_string();
            let expected = if malformed_first {
                "legacy16"
            } else {
                "production validation 后"
            };
            assert!(error.contains(expected), "{error}");
        }
    }
}

#[cfg(test)]
mod merge_main_advance_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    struct MergeSite {
        root: PathBuf,
        task_head: String,
        verdict_main: String,
        review_rel: String,
    }

    impl MergeSite {
        fn new(tag: &str) -> Self {
            let root = crate::util::test_scratch_dir(&format!("b172-main-advance-{tag}"));
            git(&root, &["init", "-q"]);
            git(&root, &["config", "user.name", "orch test"]);
            git(
                &root,
                &["config", "user.email", "orch-test@example.invalid"],
            );
            fs::write(root.join("README.md"), "base\n").unwrap();
            fs::write(root.join(".gitignore"), ".worktrees/\n").unwrap();
            git(&root, &["add", "README.md", ".gitignore"]);
            git(&root, &["commit", "-q", "-m", "base"]);
            git(&root, &["branch", "-M", "main"]);
            git(&root, &["checkout", "-q", "-b", "task/B172T"]);
            fs::write(root.join("feature.txt"), "feature\n").unwrap();
            git(&root, &["add", "feature.txt"]);
            git(&root, &["commit", "-q", "-m", "feature"]);
            git(&root, &["checkout", "-q", "main"]);
            fs::create_dir_all(root.join(".worktrees")).unwrap();
            git(
                &root,
                &["worktree", "add", "-q", ".worktrees/B172T", "task/B172T"],
            );
            let task_head = git_output(&root, &["rev-parse", "task/B172T"]);
            let initial_main = git_output(&root, &["rev-parse", "main"]);

            for path in [
                "coordination/runtime",
                "coordination/modes",
                "coordination/rounds/r58/tasks",
                "coordination/rounds/r58/reviews",
                "coordination/rounds/r58/evidence",
                "coordination/rounds/r58/seeds/B172T",
            ] {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r58\n").unwrap();
            fs::write(
                root.join("coordination/modes/test.yaml"),
                r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw]
  capacities:
    executor-desktop: {agent: 1, quota: 1, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
budgets: {round: {maxUsd: 1, wallMinutes: 60, maxModelWakes: 2}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
            )
            .unwrap();
            fs::write(
                root.join("coordination/PROJECT-BINDING.yaml"),
                "project: {ecosystems: [test]}\nscope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n  check: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
            )
            .unwrap();
            let seed_rel = "coordination/rounds/r58/seeds/B172T/contract.rs";
            let seed_bytes = b"seed contract\n";
            fs::write(root.join(seed_rel), seed_bytes).unwrap();
            let seed_sha = hex::encode(Sha256::digest(seed_bytes));
            fs::write(
                root.join("coordination/rounds/r58/tasks/B172T.md"),
                format!(
                    "---\ntaskId: B172T\nround: r58\nagent: executor-desktop\nseedProtocol: seeded-red\nentryPoints: [feature.txt]\nseeds:\n  - {{src: {seed_rel}, target: tests/contract.rs, sha256: {seed_sha}}}\nwriteSet: [feature.txt, tests/contract.rs]\nfrozenPaths: [coordination/**]\ngates: {{fast: [testFast, check]}}\nbudgets: {{wallMinutes: 30}}\nrequiredReviews:\n  - {{role: primary, agent: executor-claw}}\nrequiredEvidence: [main-advance]\n---\n# fixture\n"
                ),
            )
            .unwrap();

            crate::plan::run_plan(&root).unwrap();
            crate::round::run_sign_off(&root, Some("B172 merge fixture")).unwrap();
            let dispatch = ledger::event(
                "DispatchIssued",
                "runtime:orch",
                Some("B172T"),
                Some("r58"),
                serde_json::json!({
                    "taskId": "B172T",
                    "agent": "executor-desktop",
                    "attemptId": "B172T-A0001",
                    "attemptNo": 1,
                    "goPath": "coordination/rounds/r58/dispatch/executor-desktop/GO-B172T-A0001.md",
                    "baseSha": initial_main,
                }),
            );
            let receipt = ledger::event(
                "CollectGateSuccessReceipt",
                "runtime:orch",
                Some("B172T"),
                Some("r58"),
                serde_json::json!({
                    "actionId": "collect-B172T-A0001",
                    "attemptId": "B172T-A0001",
                    "attemptNo": 1,
                    "agent": "executor-desktop",
                    "baseSha": initial_main,
                    "goPath": "coordination/rounds/r58/dispatch/executor-desktop/GO-B172T-A0001.md",
                    "branchSha": task_head,
                }),
            );
            let collect = ledger::event(
                "ReportCollectCompleted",
                "runtime:orch",
                Some("B172T"),
                Some("r58"),
                serde_json::json!({
                    "actionId": "collect-B172T-A0001",
                    "attemptId": "B172T-A0001",
                    "attemptNo": 1,
                    "agent": "executor-desktop",
                    "baseSha": initial_main,
                    "goPath": "coordination/rounds/r58/dispatch/executor-desktop/GO-B172T-A0001.md",
                    "branchSha": task_head,
                    "gateReceipt": receipt.event_id,
                }),
            );
            ledger::append(&root, "r58", &[dispatch, receipt, collect]).unwrap();
            let review_rel =
                "coordination/rounds/r58/reviews/B172T-A0001-primary-executor-claw.md".to_string();
            fs::write(
                root.join(&review_rel),
                format!(
                    "---\ntaskId: B172T\nround: r58\nattemptId: B172T-A0001\nrole: primary\nreviewer: executor-claw\nverdict: PASS\nreviewedHead: {task_head}\n---\nreview\n"
                ),
            )
            .unwrap();
            fs::write(
                root.join("coordination/rounds/r58/evidence/B172T-main-advance.json"),
                "{\"ok\":true}\n",
            )
            .unwrap();
            git(&root, &["add", "coordination"]);
            git(&root, &["commit", "-q", "-m", "signed merge contract"]);
            let verdict_main = git_output(&root, &["rev-parse", "main"]);
            run_root_verdict(
                &root,
                "B172T",
                "B172T-A0001",
                &task_head,
                &verdict_main,
                RootVerdict::Pass,
                None,
                false,
            )
            .unwrap();
            Self {
                root,
                task_head,
                verdict_main,
                review_rel,
            }
        }

        fn events(&self) -> Vec<EventRecord> {
            read_ledger(&self.root.join("coordination/rounds/r58/events.jsonl"))
                .unwrap()
                .events
        }

        fn authorize(&self) -> Result<RootMergeAuthorization> {
            validate_root_merge_authorization(&self.root, "r58", "B172T", &self.events())
        }

        fn commit_path(&self, rel: &str, bytes: &[u8]) {
            let path = self.root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            git(&self.root, &["add", "-A"]);
            git(&self.root, &["commit", "-q", "-m", "advance main"]);
        }
    }

    impl Drop for MergeSite {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
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

    fn git_output(root: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn production_merge_authorization_accepts_unmoved_and_coordination_only_main() {
        let unmoved = MergeSite::new("unmoved");
        unmoved.authorize().unwrap();

        let advanced = MergeSite::new("coordination");
        advanced.commit_path("coordination/BOARD.md", b"planner note\n");
        advanced.authorize().unwrap();
    }

    #[test]
    fn production_merge_authorization_rejects_compilation_and_bound_review_changes() {
        let compilation = MergeSite::new("compilation");
        compilation.commit_path("orch/crates/orch-host/src/changed.rs", b"// changed\n");
        let error = compilation.authorize().unwrap_err().to_string();
        assert!(error.contains("编译输入"), "{error}");
        assert!(
            error.contains("orch/crates/orch-host/src/changed.rs"),
            "{error}"
        );

        let review = MergeSite::new("review");
        let review_rel = review.review_rel.clone();
        review.commit_path(&review_rel, b"tampered review\n");
        let error = review.authorize().unwrap_err().to_string();
        assert!(error.contains("审查/证据"), "{error}");
        assert!(error.contains(&review_rel), "{error}");
    }

    #[test]
    fn production_merge_authorization_rejects_a_forked_main() {
        let site = MergeSite::new("fork");
        let ledger_path = site.root.join("coordination/rounds/r58/events.jsonl");
        let verdict_ledger = fs::read(&ledger_path).unwrap();
        let parent = git_output(
            &site.root,
            &["rev-parse", &format!("{}^", site.verdict_main)],
        );

        git(&site.root, &["add", "coordination/rounds/r58/events.jsonl"]);
        git(&site.root, &["commit", "-q", "-m", "temporary verdict tip"]);
        git(&site.root, &["switch", "-q", "--detach", &parent]);
        git(
            &site.root,
            &[
                "restore",
                "--source",
                &site.verdict_main,
                "--staged",
                "--worktree",
                ".",
            ],
        );
        fs::write(&ledger_path, verdict_ledger).unwrap();
        git(&site.root, &["add", "-A"]);
        git(
            &site.root,
            &["commit", "-q", "-m", "fork with same contract"],
        );
        let fork = git_output(&site.root, &["rev-parse", "HEAD"]);
        git(&site.root, &["branch", "-f", "main", &fork]);
        git(&site.root, &["switch", "-q", "main"]);

        let error = site.authorize().unwrap_err().to_string();
        assert!(error.contains("不是 actual main 的祖先"), "{error}");
    }

    #[test]
    fn wake_suffix_is_merge_only() {
        let site = MergeSite::new("wake-suffix");
        ledger::append(
            &site.root,
            "r58",
            &[ledger::event(
                "WakeIssued",
                "runtime:orch",
                None,
                Some("r58"),
                serde_json::json!({"agent": "executor-claw"}),
            )],
        )
        .unwrap();
        let events = site.events();
        let active = crate::plan::require_active_round_ir(&site.root, "r58", &events).unwrap();

        validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &site.verdict_main,
            &active,
            CommittedLedgerMode::CanonicalRootSuffix,
        )
        .unwrap();
        validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &site.verdict_main,
            &active,
            CommittedLedgerMode::CanonicalPostMergeSuffix,
        )
        .unwrap();
        for mode in [
            CommittedLedgerMode::CanonicalVerdictSuffix,
            CommittedLedgerMode::Exact,
        ] {
            assert!(validate_expected_main_contract(
                &site.root,
                "r58",
                "B172T",
                &site.verdict_main,
                &active,
                mode,
            )
            .is_err());
        }
    }

    #[test]
    fn exact_attempt_storage_audit_suffix_is_valid_after_a_root_verdict() {
        let site = MergeSite::new("storage-suffix");
        let refused = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            serde_json::json!({
                "stage": "storage",
                "availableBytes": 45 * 1024_u64.pow(3),
                "thresholdBytes": 46 * 1024_u64.pow(3),
                "entry": "gate",
                "probe": "low",
                "probeReason": null,
                "attemptId": "B172T-A0001",
            }),
        );
        let recovered = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            serde_json::json!({
                "stage": "storage",
                "state": "recovered",
                "availableBytes": 50 * 1024_u64.pow(3),
                "thresholdBytes": 46 * 1024_u64.pow(3),
                "entry": "gate",
                "probe": "ok",
                "probeReason": null,
                "attemptId": "B172T-A0001",
            }),
        );
        assert!(matches!(
            root_verdict_ledger_mode(&[refused.clone(), recovered.clone()], "r58", "B172T"),
            CommittedLedgerMode::CanonicalStorageSuffix
        ));
        ledger::append(&site.root, "r58", &[refused, recovered]).unwrap();
        let events = site.events();
        let active = crate::plan::require_active_round_ir(&site.root, "r58", &events).unwrap();
        assert!(matches!(
            root_verdict_ledger_mode(&events, "r58", "B172T"),
            CommittedLedgerMode::CanonicalVerdictSuffix
        ));
        validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &site.verdict_main,
            &active,
            CommittedLedgerMode::CanonicalVerdictSuffix,
        )
        .unwrap();
        assert!(validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &site.verdict_main,
            &active,
            CommittedLedgerMode::Exact,
        )
        .is_err());
    }

    #[test]
    fn refused_verdict_retry_recovers_and_reaches_the_real_gate_spawn() {
        let site = MergeSite::new("storage-refusal-retry-spawn");
        let log_path = site.root.join(
            "coordination/runtime/logs/B172T-B172T-A0001-root-verdict-gate-testFast.log",
        );
        // `MergeSite::new` normally lands one root verdict for its authorization tests. Rewind
        // only this isolated fixture's two mirrored ledger arms to its committed pre-verdict blob,
        // so the following calls exercise the real first-attempt and retry paths.
        let committed_ledger = committed_regular_blob_bytes(
            &site.root,
            &site.verdict_main,
            "coordination/rounds/r58/events.jsonl",
            "storage retry fixture ledger",
        )
        .unwrap();
        fs::write(
            site.root.join("coordination/rounds/r58/events.jsonl"),
            &committed_ledger,
        )
        .unwrap();
        fs::write(
            site.root.join("coordination/runtime/ledger-wal/r58.jsonl"),
            &committed_ledger,
        )
        .unwrap();
        let _ = fs::remove_file(&log_path);
        fs::create_dir_all(site.root.join(".orch")).unwrap();
        fs::write(
            site.root.join(".orch/machine.yaml"),
            format!("storage:\n  floorBytes: {}\n", u64::MAX),
        )
        .unwrap();

        let first = run_root_verdict(
            &site.root,
            "B172T",
            "B172T-A0001",
            &site.task_head,
            &site.verdict_main,
            RootVerdict::Pass,
            None,
            true,
        );
        assert!(first.is_err());
        assert!(!log_path.exists(), "refusal must happen before gate log/spawn");
        let events = site.events();
        assert!(events.iter().any(|event| {
            ledger::canonical_gate_storage_audit_event(event).is_some_and(|audit| {
                audit.identity
                    == ledger::GateAuditIdentity::Attempt {
                        task_id: "B172T",
                        attempt_id: "B172T-A0001",
                    }
                    && !audit.recovered
            })
        }));

        fs::remove_file(site.root.join(".orch/machine.yaml")).unwrap();
        let other_refusal = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("BOTHER"),
            Some("r58"),
            serde_json::json!({
                "stage": "storage",
                "availableBytes": 45 * 1024_u64.pow(3),
                "thresholdBytes": 46 * 1024_u64.pow(3),
                "entry": "gate",
                "probe": "low",
                "probeReason": null,
                "attemptId": "BOTHER-A0001",
            }),
        );
        let other_recovery = ledger::event(
            "EscalationRaised",
            "runtime:orch",
            Some("BOTHER"),
            Some("r58"),
            serde_json::json!({
                "stage": "storage",
                "state": "recovered",
                "availableBytes": 50 * 1024_u64.pow(3),
                "thresholdBytes": 46 * 1024_u64.pow(3),
                "entry": "gate",
                "probe": "ok",
                "probeReason": null,
                "attemptId": "BOTHER-A0001",
            }),
        );
        ledger::append_storage_audit(&site.root, "r58", other_refusal).unwrap();
        ledger::append_storage_audit(&site.root, "r58", other_recovery).unwrap();

        let retry = run_root_verdict(
            &site.root,
            "B172T",
            "B172T-A0001",
            &site.task_head,
            &site.verdict_main,
            RootVerdict::Pass,
            None,
            true,
        )
        .unwrap();
        assert!(retry.gates.iter().any(|gate| gate.name == "testFast"));
        assert!(log_path.is_file(), "retry must reach the real gate spawn marker");
        let events = site.events();
        assert!(events.iter().any(|event| {
            ledger::canonical_gate_storage_audit_event(event).is_some_and(|audit| {
                audit.identity
                    == ledger::GateAuditIdentity::Attempt {
                        task_id: "B172T",
                        attempt_id: "B172T-A0001",
                    }
                    && audit.recovered
            })
        }));
    }

    #[test]
    fn storage_suffix_accepts_same_round_other_task_and_rejects_cross_round_or_smuggling() {
        let site = MergeSite::new("storage-suffix-scope");
        let current_ledger = fs::read(
            site.root
                .join("coordination/rounds/r58/events.jsonl"),
        )
        .unwrap();
        site.commit_path("coordination/rounds/r58/events.jsonl", &current_ledger);
        let expected_main = git_output(&site.root, &["rev-parse", "main"]);
        let committed = fs::read(
            site.root
                .join("coordination/rounds/r58/events.jsonl"),
        )
        .unwrap();
        let active = crate::plan::require_active_round_ir(&site.root, "r58", &site.events()).unwrap();

        let storage_event = |task: &str, round: &str| {
            ledger::event(
                "EscalationRaised",
                "runtime:orch",
                Some(task),
                Some(round),
                serde_json::json!({
                    "stage": "storage",
                    "availableBytes": 45 * 1024_u64.pow(3),
                    "thresholdBytes": 46 * 1024_u64.pow(3),
                    "entry": "gate",
                    "probe": "low",
                    "probeReason": null,
                    "attemptId": "BOTHER-A0001",
                }),
            )
        };
        let write_suffix = |event: EventRecord| {
            let mut bytes = committed.clone();
            serde_json::to_writer(&mut bytes, &event).unwrap();
            bytes.push(b'\n');
            fs::write(
                site.root
                    .join("coordination/rounds/r58/events.jsonl"),
                bytes,
            )
            .unwrap();
        };

        write_suffix(storage_event("BOTHER", "r58"));
        for mode in [
            CommittedLedgerMode::CanonicalStorageSuffix,
            CommittedLedgerMode::CanonicalVerdictSuffix,
            CommittedLedgerMode::CanonicalRootSuffix,
            CommittedLedgerMode::CanonicalPostMergeSuffix,
        ] {
            validate_expected_main_contract(
                &site.root,
                "r58",
                "B172T",
                &expected_main,
                &active,
                mode,
            )
            .unwrap();
        }
        assert!(validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &expected_main,
            &active,
            CommittedLedgerMode::Exact,
        )
        .is_err());

        write_suffix(storage_event("BOTHER", "r59"));
        assert!(validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &expected_main,
            &active,
            CommittedLedgerMode::CanonicalStorageSuffix,
        )
        .is_err());

        let mut smuggled = storage_event("BOTHER", "r58");
        smuggled.payload.as_mut().unwrap()["mergeSha"] = serde_json::json!(expected_main);
        write_suffix(smuggled);
        assert!(validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &expected_main,
            &active,
            CommittedLedgerMode::CanonicalStorageSuffix,
        )
        .is_err());
    }

    #[test]
    fn action_scoped_backend_receipt_is_a_strict_merge_only_suffix() {
        let site = MergeSite::new("backend-receipt-suffix");
        let wake_id = "wake-backend-suffix";
        let continuation = "implementation:r58:B172T:B172T-A0001:executor-desktop";
        let request_digest = "a".repeat(64);
        let rendered_digest = "b".repeat(64);
        let log_path = site
            .root
            .join("coordination/runtime/logs/wake.jsonl")
            .display()
            .to_string();
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            serde_json::json!({
                "wakeId": wake_id,
                "continuationId": continuation,
                "attemptId": "B172T-A0001",
                "agent": "executor-desktop",
                "providerKind": "codex",
                "requestMessageSha256": request_digest,
                "renderedMessageSha256": rendered_digest,
                "requestSessionId": null,
                "backendState": "pending",
                "logPath": log_path,
                "probeOffset": 0,
            }),
        );
        let receipt = ledger::event(
            "AgentEventReceived",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            serde_json::json!({
                "agentEvent": "wake-backend-receipt",
                "actionId": wake_id,
                "wakeId": wake_id,
                "continuationId": continuation,
                "attemptId": "B172T-A0001",
                "agent": "executor-desktop",
                "providerKind": "codex",
                "requestMessageSha256": request_digest,
                "renderedMessageSha256": rendered_digest,
                "receiptKind": "codex",
                "requestSessionId": null,
                "observedSessionId": "thread-one",
                "logPath": log_path,
                "probeOffset": 0,
                "probeEnd": 10,
                "windowSha256": "c".repeat(64),
                "backendState": "accepted",
            }),
        );
        assert!(
            canonical_wake_backend_receipt(&receipt, std::slice::from_ref(&wake), "r58").unwrap()
        );
        for key in [
            "wakeId",
            "attemptId",
            "agent",
            "providerKind",
            "logPath",
            "requestMessageSha256",
            "renderedMessageSha256",
            "windowSha256",
        ] {
            let mut mutated = receipt.clone();
            mutated.payload.as_mut().unwrap()[key] = serde_json::json!("tampered");
            assert!(
                canonical_wake_backend_receipt(&mutated, std::slice::from_ref(&wake), "r58")
                    .is_err(),
                "tampered {key} must fail closed"
            );
        }
        for key in [
            "actionId",
            "continuationId",
            "receiptKind",
            "observedSessionId",
            "probeEnd",
        ] {
            let mut missing = receipt.clone();
            missing
                .payload
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                canonical_wake_backend_receipt(&missing, std::slice::from_ref(&wake), "r58")
                    .is_err(),
                "missing {key} must fail closed"
            );
        }
        let mut wrong_session = receipt.clone();
        wrong_session.payload.as_mut().unwrap()["requestSessionId"] =
            serde_json::json!("invented-session");
        assert!(
            canonical_wake_backend_receipt(&wrong_session, std::slice::from_ref(&wake), "r58")
                .is_err()
        );
        assert!(canonical_wake_backend_receipt(&receipt, &[], "r58").is_err());
        assert!(
            canonical_wake_backend_receipt(&receipt, &[wake.clone(), receipt.clone()], "r58")
                .is_err()
        );
        let ordinary_agent_event = ledger::event(
            "AgentEventReceived",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            serde_json::json!({"agentEvent": "ordinary"}),
        );
        assert!(!canonical_wake_backend_receipt(
            &ordinary_agent_event,
            std::slice::from_ref(&wake),
            "r58"
        )
        .unwrap());
        ledger::append(&site.root, "r58", &[wake, receipt]).unwrap();
        let events = site.events();
        let active = crate::plan::require_active_round_ir(&site.root, "r58", &events).unwrap();
        for mode in [
            CommittedLedgerMode::CanonicalRootSuffix,
            CommittedLedgerMode::CanonicalPostMergeSuffix,
        ] {
            validate_expected_main_contract(
                &site.root,
                "r58",
                "B172T",
                &site.verdict_main,
                &active,
                mode,
            )
            .unwrap();
        }
        assert!(validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &site.verdict_main,
            &active,
            CommittedLedgerMode::CanonicalVerdictSuffix,
        )
        .is_err());
    }

    #[test]
    fn fixture_task_head_remains_fixed() {
        let site = MergeSite::new("fixed-head");
        assert_eq!(
            git_output(&site.root, &["rev-parse", "task/B172T"]),
            site.task_head
        );
    }
}
