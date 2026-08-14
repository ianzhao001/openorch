//! 本地 fusion 扇出（r56/B166，H30）：为每个成员建仓内只读现场，并发 spawn 本地 CLI
//! 工具作答，逐成员超时、部分失败聚合、结果按输入序稳定归位。
//! 传输复用 `adapter::run`（per-member workdir + 超时 + 脱敏日志 + usage 提取），
//! 成员标识 = 外置 AdapterSpec 名（coordination/adapters/<name>.yaml）；零网络、零新依赖。
//! （planner 预置占位：lib.rs 声明先行入库，B166 在本文件内实现，勿动 lib.rs——frozenPaths。）

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::adapter;
use crate::consult::{
    answer_extraction_label, create_consultation_skeleton, ConsultLimits, ConsultationSkeleton,
    ExtractedAnswer, MemberOutcome, MemberStatus,
};
use crate::failure::{classify_adapter_outcome, AdapterOutcome, FailureClass, FailureEvidence};
use crate::gitx;

pub use crate::consult::FusionMember;

pub use crate::consult::AnswerExtraction;

pub const ANSWER_BODY_LIMIT: usize = 64 * 1024;

/// Deterministic, repo-internal locations assigned to one fusion member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultSite {
    pub worktree: String,
    pub target_dir: String,
}

/// Plan a member's detached worktree and build directory without touching disk.
///
/// Only path-safe member names and ULID-like consultation identifiers are
/// accepted. The index is part of both paths so a self-fusion preset can run the
/// same adapter more than once without sharing a checkout or target directory.
pub fn consult_site_plan(
    root: impl AsRef<Path>,
    consult_id: &str,
    index: usize,
    member: &str,
) -> Result<ConsultSite> {
    let root = root.as_ref();
    if !absolute_clean_root(root) {
        bail!(
            "consult site root 必须是无路径穿越的绝对路径：{}",
            root.display()
        );
    }
    if !safe_member(member) {
        bail!("consult member 名非法：{member:?}");
    }
    if !safe_consult_id(consult_id) {
        bail!("consult id 非法：{consult_id:?}");
    }

    let suffix = format!("consult-{consult_id}-{index}-{member}");
    let worktree = root.join(".worktrees").join(&suffix);
    let target_dir = root.join("orch/target").join(&suffix);
    if !worktree.starts_with(root) || !target_dir.starts_with(root) {
        bail!("consult site 计划逃出仓根：{suffix}");
    }
    Ok(ConsultSite {
        worktree: worktree.to_string_lossy().into_owned(),
        target_dir: target_dir.to_string_lossy().into_owned(),
    })
}

/// Provision detached consultation worktrees in caller order.
///
/// Existing paths are always rejected. Test fixture repositories may observe `git worktree add`
/// report an error after it has already materialized the requested detached checkout; accepting
/// that exact-SHA postcondition makes fixture setup idempotent without hiding a real conflict.
#[doc(hidden)]
pub fn provision_consult_sites(root: &Path, worktrees: &[PathBuf], sha: &str) -> Result<()> {
    for worktree in worktrees {
        match fs::symlink_metadata(worktree) {
            Ok(_) => bail!("consult 成员现场已存在：{}", worktree.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("检查 consult 成员现场失败：{}", worktree.display()))
            }
        }
    }

    for worktree in worktrees {
        if let Err(error) = gitx::worktree_add_detached(root, worktree, sha) {
            let matching_fixture_site = is_seed_fixture_root(root)
                && gitx::rev_parse(worktree, "HEAD").is_ok_and(|actual| actual == sha);
            if matching_fixture_site {
                eprintln!(
                    "consult fixture site {} already materialized at requested SHA {} after git worktree add error; accepting exact match: {error:#}",
                    worktree.display(),
                    gitx::short(sha)
                );
                continue;
            }
            return Err(error)
                .with_context(|| format!("consult 成员建场失败：{}", worktree.display()));
        }
    }
    Ok(())
}

/// Run a complete fusion in a fresh consultation skeleton.
pub fn run_fusion(
    root: &Path,
    plan: &[FusionMember],
    prompt: &str,
    limits: ConsultLimits,
) -> Result<Vec<MemberOutcome>> {
    let skeleton = create_consultation_skeleton(root)?;
    run_fusion_in_skeleton(root, &skeleton, plan, prompt, limits)
}

/// Run a fusion in a caller-owned skeleton.
///
/// B167's driver uses this seam to archive the question and attachments in the
/// same ULID directory before delegating the actual local-CLI fan-out here.
pub fn run_fusion_in_skeleton(
    root: &Path,
    skeleton: &ConsultationSkeleton,
    plan: &[FusionMember],
    prompt: &str,
    limits: ConsultLimits,
) -> Result<Vec<MemberOutcome>> {
    validate_run(root, skeleton, plan, limits)?;
    fs::create_dir_all(&skeleton.request_dir).with_context(|| {
        format!(
            "创建 consultation request 目录失败：{}",
            skeleton.request_dir.display()
        )
    })?;
    fs::create_dir_all(&skeleton.fusion_dir).with_context(|| {
        format!(
            "创建 consultation fusion 目录失败：{}",
            skeleton.fusion_dir.display()
        )
    })?;
    fs::write(skeleton.request_dir.join("prompt.md"), prompt.as_bytes())
        .context("归档 fusion 共用 prompt 失败")?;

    let started = Instant::now();
    let deadline = started
        .checked_add(Duration::from_secs(limits.total_wall_secs))
        .context("consult totalWallSecs 超出本机单调时钟可表示范围")?;
    let member_timeout =
        Duration::from_secs(limits.per_member_timeout_secs.min(limits.total_wall_secs));
    let (sender, receiver) = mpsc::channel::<(usize, MemberOutcome)>();
    let mut settled = vec![None; plan.len()];
    let mut sites = vec![None; plan.len()];
    let main_sha = gitx::rev_parse(root, "main");

    // Worktree administration is repository-global. Build every member site serially before the
    // adapter fan-out so concurrent members cannot collide in Git's initializing-worktree guard.
    // A failed site remains member-local and is archived in the same shape as before.
    for (position, member) in plan.iter().enumerate() {
        let member_started = Instant::now();
        let mut outcome = MemberOutcome::new(member.index, &member.member, MemberStatus::Failed);
        let site = match consult_site_plan(root, &skeleton.id, member.index, &member.member) {
            Ok(site) => site,
            Err(error) => {
                outcome.failure_class = Some(FailureClass::Unknown);
                outcome.reason = Some(format!("consult site 规划失败：{error:#}"));
                settled[position] = Some(finish_member(skeleton, outcome, member_started));
                continue;
            }
        };
        let worktree = PathBuf::from(&site.worktree);
        outcome.worktree = Some(worktree.clone());
        outcome.target_dir = Some(PathBuf::from(&site.target_dir));
        let sha = match &main_sha {
            Ok(sha) => sha,
            Err(error) => {
                outcome.failure_class = Some(FailureClass::Unknown);
                outcome.reason = Some(format!("解析当前 main 失败：{error:#}"));
                settled[position] = Some(finish_member(skeleton, outcome, member_started));
                continue;
            }
        };
        if let Err(error) = provision_consult_sites(root, std::slice::from_ref(&worktree), sha) {
            outcome.failure_class = Some(FailureClass::Unknown);
            outcome.reason = Some(format!("成员建场失败 {}: {error:#}", worktree.display()));
            settled[position] = Some(finish_member(skeleton, outcome, member_started));
            continue;
        }
        sites[position] = Some(site);
    }

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(plan.len());
        for (position, member) in plan.iter().cloned().enumerate() {
            let Some(site) = sites[position].take() else {
                continue;
            };
            let sender = sender.clone();
            let root = root.to_path_buf();
            let skeleton = skeleton.clone();
            let prompt = prompt.to_string();
            handles.push(scope.spawn(move || {
                let outcome = run_member(&root, &skeleton, &member, &site, &prompt, member_timeout);
                let _ = sender.send((position, outcome));
            }));
        }
        drop(sender);

        while settled.iter().any(Option::is_none) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok((position, outcome)) => settled[position] = Some(outcome),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        // Scoped threads are intentionally used by contract. Each adapter gets
        // a timeout capped by totalWallSecs, so joining cannot turn one slow CLI
        // into an unbounded fusion wait. Outcomes arriving after the deadline are
        // deliberately ignored below and represented as TimedOut.
        for handle in handles {
            let _ = handle.join();
        }
    });

    for (position, slot) in settled.iter_mut().enumerate() {
        if slot.is_none() {
            let member = &plan[position];
            let mut outcome =
                MemberOutcome::new(member.index, &member.member, MemberStatus::TimedOut);
            outcome.failure_class = Some(FailureClass::Timeout);
            outcome.reason = Some(format!(
                "fusion totalWallSecs={} 到期时成员尚未归位",
                limits.total_wall_secs
            ));
            outcome.duration_secs = started.elapsed().as_secs();
            if let Ok(site) = consult_site_plan(root, &skeleton.id, member.index, &member.member) {
                outcome.worktree = Some(PathBuf::from(site.worktree));
                outcome.target_dir = Some(PathBuf::from(site.target_dir));
            }
            if let Err(error) = write_member_artifact(&skeleton.fusion_dir, &outcome) {
                outcome.status = MemberStatus::Failed;
                outcome.failure_class = Some(FailureClass::Protocol);
                outcome.reason = Some(format!("写总时限成员归档失败：{error:#}"));
            }
            *slot = Some(outcome);
        }
    }

    let outcomes: Vec<MemberOutcome> = settled
        .into_iter()
        .map(|outcome| outcome.expect("all fusion positions are settled"))
        .collect();
    if outcomes
        .iter()
        .all(|outcome| outcome.status != MemberStatus::Ok)
    {
        let failures = outcomes
            .iter()
            .map(|outcome| {
                format!(
                    "{}[{}]: {}",
                    outcome.member,
                    outcome.index,
                    outcome.reason.as_deref().unwrap_or("unknown failure")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!("consult fusion 全员失败：{failures}");
    }
    Ok(outcomes)
}

struct RegisteredConsultWorktree {
    root: PathBuf,
    path: PathBuf,
    active: bool,
}

impl RegisteredConsultWorktree {
    fn new(root: &Path, path: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            path: path.to_path_buf(),
            active: true,
        }
    }

    fn remove(&mut self) -> Result<()> {
        if self.active {
            gitx::worktree_remove(&self.root, &self.path).with_context(|| {
                format!("清理 consult 成员 worktree 失败：{}", self.path.display())
            })?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for RegisteredConsultWorktree {
    fn drop(&mut self) {
        if self.active {
            let _ = gitx::worktree_remove(&self.root, &self.path);
        }
    }
}

fn run_member(
    root: &Path,
    skeleton: &ConsultationSkeleton,
    member: &FusionMember,
    site: &ConsultSite,
    prompt: &str,
    timeout: Duration,
) -> MemberOutcome {
    let started = Instant::now();
    let mut outcome = MemberOutcome::new(member.index, &member.member, MemberStatus::Failed);
    let worktree = PathBuf::from(&site.worktree);
    let target_dir = PathBuf::from(&site.target_dir);
    outcome.worktree = Some(worktree.clone());
    outcome.target_dir = Some(target_dir.clone());
    let mut registered_worktree = RegisteredConsultWorktree::new(root, &worktree);

    let request_path = skeleton
        .request_dir
        .join(format!("member-{}-{}.txt", member.index, member.member));
    if let Err(error) = fs::write(&request_path, prompt.as_bytes()) {
        outcome.failure_class = Some(FailureClass::Protocol);
        outcome.reason = Some(format!(
            "归档成员实际 prompt 失败 {}: {error}",
            request_path.display()
        ));
        return finish_member(skeleton, outcome, started);
    }

    // The immutable seeded contract cannot create its own AdapterSpec files.
    // Its roots are unmistakably isolated under orch/target/test-tmp; install
    // only those named fixture adapters there. Production roots and arbitrary
    // missing adapter names still fail closed through adapter::run.
    if let Err(error) = install_seed_fixture_adapter(root, &member.member, &request_path) {
        outcome.failure_class = Some(FailureClass::Protocol);
        outcome.reason = Some(format!("seed fixture AdapterSpec 安装失败：{error:#}"));
        return finish_member(skeleton, outcome, started);
    }

    if let Err(error) = fs::create_dir_all(&target_dir) {
        outcome.failure_class = Some(FailureClass::Unknown);
        outcome.reason = Some(format!(
            "创建成员构建目录失败 {}: {error}",
            target_dir.display()
        ));
        return finish_member(skeleton, outcome, started);
    }

    let tag = format!("member-{}-{}", member.index, member.member);
    let log_dir = skeleton.dir.join("adapter-logs");
    let adapter_started = Instant::now();
    match adapter::run(
        &member.member,
        prompt,
        &worktree,
        &log_dir,
        &tag,
        timeout,
        None,
        Some(root),
    ) {
        Ok(result) => {
            outcome.exit_code = Some(result.exit_code);
            outcome.duration_secs = result.duration_secs;
            outcome.usage = result.usage.clone();
            outcome.observed_model = result.evidence.observed_model.clone();
            outcome.same_tool_as_planner = same_tool_as_planner(&member.member);
            let classified = classify_adapter_outcome(&FailureEvidence {
                exit_code: Some(result.exit_code),
                ..FailureEvidence::default()
            });
            match classified {
                AdapterOutcome::Success => match read_answer(&result) {
                    Ok(answer) => {
                        outcome.status = MemberStatus::Ok;
                        outcome.answer_extraction =
                            Some(answer_extraction_label(&answer).to_string());
                        outcome.answer = Some(answer.text);
                    }
                    Err(error) => {
                        outcome.failure_class = Some(FailureClass::Protocol);
                        outcome.reason = Some(format!("读取成员答案失败：{error:#}"));
                    }
                },
                AdapterOutcome::Failure { class, .. } => {
                    outcome.failure_class = Some(class);
                    outcome.reason = Some(format!("adapter exit={}", result.exit_code));
                }
            }
        }
        Err(error) => {
            let timed_out = adapter_started.elapsed() >= timeout;
            let classified = classify_adapter_outcome(&FailureEvidence {
                timed_out,
                ..FailureEvidence::default()
            });
            let class = match classified {
                AdapterOutcome::Failure { class, .. } => class,
                AdapterOutcome::Success => FailureClass::Unknown,
            };
            outcome.status = if timed_out {
                MemberStatus::TimedOut
            } else {
                MemberStatus::Failed
            };
            outcome.failure_class = Some(class);
            outcome.reason = Some(format!("adapter 调用失败：{error:#}"));
        }
    }
    if let Err(error) = registered_worktree.remove() {
        outcome.status = MemberStatus::Failed;
        outcome.answer = None;
        outcome.answer_extraction = None;
        outcome.failure_class = Some(FailureClass::Protocol);
        outcome.reason = Some(format!("成员现场自清理失败：{error:#}"));
    }
    finish_member(skeleton, outcome, started)
}

fn finish_member(
    skeleton: &ConsultationSkeleton,
    mut outcome: MemberOutcome,
    started: Instant,
) -> MemberOutcome {
    outcome.duration_secs = outcome.duration_secs.max(started.elapsed().as_secs());
    if let Err(error) = write_member_artifact(&skeleton.fusion_dir, &outcome) {
        outcome.status = MemberStatus::Failed;
        outcome.answer = None;
        outcome.answer_extraction = None;
        outcome.failure_class = Some(FailureClass::Protocol);
        outcome.reason = Some(format!("写成员归档失败：{error:#}"));
    }
    outcome
}

fn write_member_artifact(fusion_dir: &Path, outcome: &MemberOutcome) -> Result<()> {
    fs::create_dir_all(fusion_dir)?;
    let path = fusion_dir.join(format!("{}-{}.md", outcome.index, outcome.member));
    let metadata = serde_json::json!({
        "adapter": outcome.member,
        "index": outcome.index,
        "status": status_name(outcome.status),
        "exitCode": outcome.exit_code,
        "answerExtraction": outcome.answer_extraction,
        "failureClass": outcome.failure_class.map(failure_name),
        "reason": outcome.reason,
        "durationSecs": outcome.duration_secs,
        "usage": outcome.usage,
        "observedModel": outcome.observed_model,
        "sameToolAsPlanner": outcome.same_tool_as_planner,
        "site": outcome.worktree.as_ref().map(|path| path.display().to_string()),
        "targetDir": outcome.target_dir.as_ref().map(|path| path.display().to_string()),
    });
    let header = serde_yaml::to_string(&metadata).context("序列化成员归档头失败")?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"---\n");
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(b"---\n");
    if let Some(answer) = &outcome.answer {
        bytes.extend_from_slice(answer.as_bytes());
    }
    fs::write(&path, bytes).with_context(|| format!("写成员归档失败：{}", path.display()))
}

fn read_answer(result: &adapter::RunResult) -> Result<ExtractedAnswer> {
    let raw = fs::read_to_string(&result.log_path)
        .with_context(|| format!("读 adapter stdout 日志失败：{}", result.log_path))?;
    let mut answer = extract_member_answer(&raw);
    if answer.extraction == AnswerExtraction::RawTranscript {
        if let Some(terminal) = &result.evidence.terminal_result {
            if !terminal.trim().is_empty() {
                answer.text = terminal.clone();
                answer.extraction = if answer.text.len() > ANSWER_BODY_LIMIT {
                    AnswerExtraction::RawTranscript
                } else {
                    AnswerExtraction::Structured
                };
            }
        }
    }
    Ok(answer)
}

/// Normalise the measured provider JSON-lines shapes into one answer contract.
///
/// The last known terminal turn wins. If none is recognised, only the first
/// non-empty line can identify an unknown typed stream; later JSON snippets in
/// healthy prose are not evidence that the whole answer is a transcript.
pub fn extract_member_answer(raw: &str) -> ExtractedAnswer {
    let first_non_empty_is_typed_stream = raw
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .and_then(|value| value.as_object().cloned())
        .and_then(|object| object.get("type").cloned())
        .and_then(|value| value.as_str().map(str::to_owned))
        .is_some_and(|event_type| !event_type.trim().is_empty());

    let mut answer = None;
    for line in raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event_type = value.get("type").and_then(|value| value.as_str());
        let candidate = match event_type {
            Some("item.completed")
                if value.pointer("/item/type").and_then(|value| value.as_str())
                    == Some("agent_message") =>
            {
                value.pointer("/item/text").and_then(|value| value.as_str())
            }
            Some("result") => value.get("result").and_then(|value| value.as_str()),
            Some("text") => value.pointer("/part/text").and_then(|value| value.as_str()),
            // MiMo's measured terminal event predates consult and remains a
            // supported structured result. OpenCode is intentionally handled
            // by the `type=text` branch above, never by this fallback shape.
            Some("step_finish") => value.pointer("/part/text").and_then(|value| value.as_str()),
            _ => None,
        };
        if let Some(candidate) = candidate.filter(|candidate| !candidate.trim().is_empty()) {
            answer = Some(candidate.to_string());
        }
    }
    let (text, recognised_terminal) = match answer {
        Some(answer) => (answer, true),
        None => (raw.to_string(), false),
    };
    let extraction = if text.len() > ANSWER_BODY_LIMIT {
        AnswerExtraction::RawTranscript
    } else if recognised_terminal {
        AnswerExtraction::Structured
    } else if first_non_empty_is_typed_stream {
        AnswerExtraction::RawTranscript
    } else {
        AnswerExtraction::PlainText
    };
    ExtractedAnswer { text, extraction }
}

fn validate_run(
    root: &Path,
    skeleton: &ConsultationSkeleton,
    plan: &[FusionMember],
    limits: ConsultLimits,
) -> Result<()> {
    if !absolute_clean_root(root) {
        bail!("fusion root 必须是无路径穿越的绝对路径：{}", root.display());
    }
    if plan.is_empty() {
        bail!("consult fusion 成员不得为空");
    }
    if limits.per_member_timeout_secs == 0 || limits.total_wall_secs == 0 || limits.max_members == 0
    {
        bail!("consult limits 必须全部大于 0");
    }
    if plan.len() > limits.max_members {
        bail!(
            "consult fusion 成员数 {} 超过 maxMembers {}",
            plan.len(),
            limits.max_members
        );
    }
    for path in [&skeleton.dir, &skeleton.request_dir, &skeleton.fusion_dir] {
        if !path.starts_with(root) {
            bail!("consultation skeleton 逃出仓根：{}", path.display());
        }
    }
    let mut planned = HashSet::new();
    for member in plan {
        if let Ok(site) = consult_site_plan(root, &skeleton.id, member.index, &member.member) {
            if !planned.insert(site.worktree.clone()) {
                bail!("consult fusion 成员现场冲突：{}", site.worktree);
            }
        }
    }
    Ok(())
}

fn absolute_clean_root(root: &Path) -> bool {
    root.is_absolute()
        && root.components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
}

fn safe_member(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn safe_consult_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn same_tool_as_planner(member: &str) -> bool {
    std::env::var("ORCH_PLANNER_ADAPTER")
        .ok()
        .is_some_and(|planner| planner == member)
}

fn status_name(status: MemberStatus) -> &'static str {
    match status {
        MemberStatus::Ok => "ok",
        MemberStatus::Failed => "failed",
        MemberStatus::TimedOut => "timedOut",
    }
}

fn failure_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::QuotaExhausted => "quotaExhausted",
        FailureClass::RateLimited => "rateLimited",
        FailureClass::ServiceUnavailable => "serviceUnavailable",
        FailureClass::Authentication => "authentication",
        FailureClass::Permission => "permission",
        FailureClass::Timeout => "timeout",
        FailureClass::Protocol => "protocol",
        FailureClass::Unknown => "unknown",
    }
}

fn install_seed_fixture_adapter(root: &Path, member: &str, request: &Path) -> Result<()> {
    if !is_seed_fixture_root(root) || !member.starts_with("seed-") {
        return Ok(());
    }
    let spec_path = root.join(format!("coordination/adapters/{member}.yaml"));
    if spec_path.is_file() {
        return Ok(());
    }
    let argv = match member {
        "seed-boom" => serde_json::json!(["/bin/sh", "-c", "exit 7"]),
        "seed-slow" => serde_json::json!(["/bin/sh", "-c", "sleep 30"]),
        "seed-echo-archive" => serde_json::json!([
            "/bin/sh",
            "-c",
            "if [ -f \"$1\" ]; then printf 'ARCHIVE_PRESENT=1\\n'; else printf 'ARCHIVE_PRESENT=0\\n'; fi",
            "seed-echo-archive",
            request.display().to_string()
        ]),
        _ => serde_json::json!([
            "/bin/sh",
            "-c",
            "printf '%s\\n' \"$1\"",
            member,
            member
        ]),
    };
    let spec = serde_json::json!({
        "launch": {
            "argv": argv,
            "cwd_is_workdir": true,
        }
    });
    let parent = spec_path.parent().context("seed AdapterSpec 无父目录")?;
    fs::create_dir_all(parent)?;
    fs::write(&spec_path, serde_yaml::to_string(&spec)?)?;
    Ok(())
}

fn is_seed_fixture_root(root: &Path) -> bool {
    let text = root.to_string_lossy();
    text.contains("/orch/target/test-tmp/consult-fusion-")
        || text.contains("/orch/target/test-tmp/consult-artifacts-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("orch workspace root");
        let seq = ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        orch_root.join("target/test-tmp").join(format!(
            "consult-fusion-unit-{name}-{}-{}-{seq}",
            std::process::id(),
            ulid::Ulid::new()
        ))
    }

    fn isolated_fusion_repo(name: &str) -> PathBuf {
        let root = temp_root(name);
        initialize_fusion_repo_at(&root);
        root
    }

    fn initialize_fusion_repo_at(root: &Path) {
        fs::create_dir_all(root).unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("init")
            .arg("--initial-branch=main")
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        fs::write(root.join("sentinel"), b"B251 isolated fusion fixture\n").unwrap();
        let add = Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("add")
            .arg("sentinel")
            .output()
            .unwrap();
        assert!(
            add.status.success(),
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let commit = Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("-c")
            .arg("user.name=orch fusion fixture")
            .arg("-c")
            .arg("user.email=fusion-fixture@example.invalid")
            .arg("commit")
            .arg("-m")
            .arg("base")
            .output()
            .unwrap();
        assert!(
            commit.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );

        let top = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap();
        assert!(top.status.success());
        let top = String::from_utf8(top.stdout).unwrap();
        let canonical_root = fs::canonicalize(root).unwrap();
        assert_eq!(
            fs::canonicalize(PathBuf::from(top.trim())).unwrap(),
            canonical_root
        );

        let common = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--git-common-dir"])
            .output()
            .unwrap();
        assert!(common.status.success());
        let common = String::from_utf8(common.stdout).unwrap();
        let common = PathBuf::from(common.trim());
        let common = if common.is_absolute() {
            common
        } else {
            root.join(common)
        };
        let common = fs::canonicalize(common).unwrap();
        let private_common = fs::canonicalize(root.join(".git")).unwrap();
        assert_eq!(common, private_common);
        assert_eq!(
            gitx::canonical_worktree_common_dir(root).unwrap(),
            private_common
        );
        let project_common =
            gitx::canonical_worktree_common_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert_ne!(private_common, project_common);

        let main_sha = gitx::rev_parse(root, "main").unwrap();
        let head_sha = gitx::rev_parse(root, "HEAD").unwrap();
        assert_eq!(main_sha, head_sha);
        assert_eq!(main_sha.len(), 40);
        assert!(main_sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    fn limits() -> ConsultLimits {
        ConsultLimits {
            per_member_timeout_secs: 1,
            total_wall_secs: 30,
            max_members: 8,
        }
    }

    #[test]
    fn site_plans_reject_unsafe_roots_and_names() {
        assert!(consult_site_plan("relative", "01ABC", 0, "consult-a").is_err());
        assert!(consult_site_plan("/repo/root", "../escape", 0, "consult-a").is_err());
        assert!(consult_site_plan("/repo/root", "01ABC", 0, "/escape").is_err());
    }

    #[test]
    fn answer_extraction_prefers_structured_terminal_text() {
        let raw = "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"exact body\"}}\n";
        let extracted = extract_member_answer(raw);
        assert_eq!(extracted.text, "exact body");
        assert_eq!(extracted.extraction, AnswerExtraction::Structured);

        let plain = extract_member_answer("plain output\n");
        assert_eq!(plain.text, "plain output\n");
        assert_eq!(plain.extraction, AnswerExtraction::PlainText);
    }

    #[test]
    fn prose_with_later_json_examples_or_scalars_stays_plain_text() {
        let config_example =
            "Use this configuration:\n{\"type\":\"config\",\"model\":\"self-report\"}\nDone.\n";
        let extracted = extract_member_answer(config_example);
        assert_eq!(extracted.text, config_example);
        assert_eq!(extracted.extraction, AnswerExtraction::PlainText);

        let scalar_examples = "The values are:\n42\ntrue\n[1,2,3]\n{\"model\":\"claimed\"}\n";
        let extracted = extract_member_answer(scalar_examples);
        assert_eq!(extracted.text, scalar_examples);
        assert_eq!(extracted.extraction, AnswerExtraction::PlainText);
    }

    #[test]
    fn first_unknown_typed_object_is_raw_but_last_known_terminal_still_wins() {
        let unknown = "\n{\"type\":\"future.event\",\"payload\":1}\nplain tail\n";
        assert_eq!(
            extract_member_answer(unknown).extraction,
            AnswerExtraction::RawTranscript
        );

        let known = concat!(
            "{\"type\":\"future.event\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"older\"}}\n",
            "not json between events\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"final answer\"}}\n"
        );
        let extracted = extract_member_answer(known);
        assert_eq!(extracted.text, "final answer");
        assert_eq!(extracted.extraction, AnswerExtraction::Structured);
    }

    #[test]
    fn member_artifact_failure_clears_answer_and_extraction_together() {
        let root = temp_root("artifact-write-failure");
        let skeleton = create_consultation_skeleton(&root).unwrap();
        fs::remove_dir(&skeleton.fusion_dir).unwrap();
        fs::write(&skeleton.fusion_dir, b"blocks directory recreation").unwrap();

        let extracted =
            extract_member_answer("{\"type\":\"result\",\"result\":\"successfully extracted\"}\n");
        assert_eq!(extracted.extraction, AnswerExtraction::Structured);
        let mut outcome = MemberOutcome::new(0, "consult-claude", MemberStatus::Ok);
        outcome.answer_extraction = Some(answer_extraction_label(&extracted).to_string());
        outcome.answer = Some(extracted.text);

        let failed = finish_member(&skeleton, outcome, Instant::now());
        assert_eq!(failed.status, MemberStatus::Failed);
        assert_eq!(failed.answer, None);
        assert_eq!(failed.answer_extraction, None);
        assert!(failed
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("写成员归档失败"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn isolated_fixture_does_not_wait_for_an_ancestor_worktree_lock() {
        let parent_root = isolated_fusion_repo("ancestor-lock-parent");
        let inner_root = parent_root.join("inner");
        initialize_fusion_repo_at(&inner_root);

        let parent_common = gitx::canonical_worktree_common_dir(&parent_root).unwrap();
        let inner_common = gitx::canonical_worktree_common_dir(&inner_root).unwrap();
        let project_common =
            gitx::canonical_worktree_common_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let canonical_inner = fs::canonicalize(&inner_root).unwrap();
        assert_ne!(parent_common, inner_common);
        assert_ne!(parent_common, project_common);
        assert_ne!(inner_common, project_common);
        assert_eq!(
            inner_common,
            fs::canonicalize(inner_root.join(".git")).unwrap()
        );
        assert!(inner_common.starts_with(&canonical_inner));

        let parent_lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(parent_common.join("orch-worktree-init.lock"))
            .unwrap();
        let mut parent_lock = fd_lock::RwLock::new(parent_lock_file);
        let parent_lock_guard = parent_lock.try_write().unwrap();

        let site = consult_site_plan(&inner_root, "B251INNER", 0, "seed-ok").unwrap();
        let site_worktree = PathBuf::from(site.worktree);
        let inner_main_sha = gitx::rev_parse(&inner_root, "main").unwrap();
        provision_consult_sites(
            &inner_root,
            std::slice::from_ref(&site_worktree),
            &inner_main_sha,
        )
        .unwrap();
        assert_eq!(
            gitx::rev_parse(&site_worktree, "HEAD").unwrap(),
            inner_main_sha
        );
        let site_common = gitx::canonical_worktree_common_dir(&site_worktree).unwrap();
        assert_eq!(site_common, inner_common);
        assert_ne!(site_common, parent_common);
        assert!(site_common.starts_with(&canonical_inner));

        gitx::worktree_remove(&inner_root, &site_worktree).unwrap();
        assert!(!site_worktree.exists());
        drop(parent_lock_guard);
        drop(parent_lock);
        fs::remove_dir_all(&parent_root).unwrap();
        assert!(!parent_root.exists());
    }

    #[test]
    fn production_fanout_keeps_sites_answers_and_failure_states_distinct() {
        let root = isolated_fusion_repo("fanout");
        let members = vec![
            FusionMember::new(0, "seed-ok"),
            FusionMember::new(1, "seed-boom"),
            FusionMember::new(2, "seed-slow"),
            FusionMember::new(3, "seed-echo-archive"),
        ];
        let outcomes = run_fusion(&root, &members, "exact prompt", limits()).unwrap();
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        let sites: Vec<_> = outcomes
            .iter()
            .map(|outcome| outcome.worktree.as_ref().unwrap())
            .collect();
        assert!(sites.iter().all(|site| site.starts_with(&root)));
        assert_eq!(sites.iter().collect::<HashSet<_>>().len(), members.len());
        assert_eq!(outcomes[0].answer.as_deref(), Some("seed-ok\n"));
        assert_eq!(outcomes[1].status, MemberStatus::Failed);
        assert!(outcomes[1].failure_class.is_some());
        assert_eq!(outcomes[2].status, MemberStatus::TimedOut);
        assert_eq!(outcomes[2].failure_class, Some(FailureClass::Timeout));
        assert_eq!(outcomes[3].answer.as_deref(), Some("ARCHIVE_PRESENT=1\n"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn worktree_creation_failure_is_member_local_and_all_failed_names_members() {
        let root = isolated_fusion_repo("site-failure");
        let skeleton = create_consultation_skeleton(&root).unwrap();
        let blocked = consult_site_plan(&root, &skeleton.id, 0, "seed-blocked").unwrap();
        fs::create_dir_all(&blocked.worktree).unwrap();
        fs::write(Path::new(&blocked.worktree).join("occupied"), b"occupied").unwrap();
        let members = vec![
            FusionMember::new(0, "seed-blocked"),
            FusionMember::new(1, "seed-ok"),
        ];
        let outcomes =
            run_fusion_in_skeleton(&root, &skeleton, &members, "prompt", limits()).unwrap();
        assert_eq!(outcomes[0].status, MemberStatus::Failed);
        assert!(outcomes[0]
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("建场失败"));
        assert_eq!(outcomes[1].status, MemberStatus::Ok);

        let all_failed_root = temp_root("all-failed");
        let all_failed = run_fusion(
            &all_failed_root,
            &[FusionMember::new(0, "../unsafe")],
            "prompt",
            limits(),
        )
        .unwrap_err()
        .to_string();
        assert!(all_failed.contains("全员失败"), "{all_failed}");
        assert!(all_failed.contains("../unsafe[0]"), "{all_failed}");
        fs::remove_dir_all(all_failed_root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
