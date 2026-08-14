//! 机检（零模型，design/06 §3）：域检(E4 merge-base) / 提交形状 / 种子字节。
//! 从 runtask 抽出为独立函数，供 run-task 与 `orch check`（故障注入验证）共用。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::{binding, card, gitx, ledger};

pub struct MechOutcome {
    pub merge_base: String,
    pub notes: Vec<String>,
    /// (target, sha256) —— 供调用方落 SeedRelocated 事件
    pub seed_digests: Vec<(String, String)>,
}

fn merge_base_matches_policy(expected: &str, actual: &str, expected_is_ancestor: bool) -> bool {
    actual == expected || expected_is_ancestor
}

/// 判断 REPORT 相对路径是否作为完整路径存在于分支 HEAD 的树清单中。
///
/// `tree_paths` 是 `git ls-tree -r --name-only <tree>` 的原始文本输出。
/// 只容忍空白行和 CRLF 的行尾 `\r`，不做子串或路径规范化匹配。
pub fn report_committed(tree_paths: &str, report_rel: &str) -> bool {
    tree_paths
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| !line.trim().is_empty())
        .any(|line| line == report_rel)
}

/// 读取分支 HEAD 的完整树路径清单。
///
/// `gitx` 当前没有公开的通用 runner/ls-tree 包装；先经 gitx 将分支解析为
/// 不可移动的 tree SHA，再按 E6 用绝对 `git -C` 执行只读 ls-tree。
pub fn tree_paths_at_head(root: &Path, branch: &str) -> Result<String> {
    let tree = gitx::rev_parse(root, &format!("{branch}^{{tree}}"))?;
    let absolute_root = root
        .canonicalize()
        .with_context(|| format!("解析仓库绝对路径失败: {}", root.display()))?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&absolute_root)
        .args([
            "-c",
            "core.quotePath=false",
            "ls-tree",
            "-r",
            "--name-only",
            &tree,
        ])
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "读取分支 HEAD 树失败: git -C {} ls-tree",
                absolute_root.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "git ls-tree 失败({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("git ls-tree 输出不是 UTF-8")
}

/// B84：本任务合法证据路径精确白名单（REPORT + 同任务 BLOCKED 兄弟）。
///
/// 仅两者返回 true：
/// 1. `candidate` 与 `report_rel` 精确相等；
/// 2. `report_rel` 精确以 `-REPORT.md` 结尾时，`candidate` 与替换为
///    `-BLOCKED.md` 的兄弟路径精确相等。
/// 禁止 contains/prefix/宽 suffix/目录级白名单；`report_rel` 不以后缀结尾时
/// 只认其精确值本身（其他任务 BLOCKED、`.bak`、SUMMARY、路径前缀伪装均拒绝）。
pub fn task_evidence_path(candidate: &str, report_rel: &str) -> bool {
    if candidate == report_rel {
        return true;
    }
    match report_rel.strip_suffix("-REPORT.md") {
        Some(stem) => candidate == format!("{stem}-BLOCKED.md"),
        None => false,
    }
}

pub fn path_guard(
    changed: &[String],
    write_set: &[String],
    frozen: &[String],
    protected: &[String],
    report_rel: &str,
) -> Result<()> {
    for f in changed {
        if card::path_matches(protected, f) && !task_evidence_path(f, report_rel) {
            bail!("protected 路径禁止修改: {f}");
        }
        let allowed = card::path_matches(write_set, f) || task_evidence_path(f, report_rel);
        if !allowed {
            bail!("文件域越界: {f}（整棒 FAIL，RELAY 铁律 4）");
        }
        if card::path_matches(frozen, f) && !task_evidence_path(f, report_rel) {
            bail!("触碰冻结路径: {f}");
        }
    }
    Ok(())
}

/// 构造机检失败事件（复用 ledger::event 通道：ULID id + RFC3339 ts）。
/// kind="MechCheckFailed"，payload={"stage": stage, "reason": reason}。
/// 供 collect 在各 bail 点「先 append 再 bail」——失败也落账（E8 补充）。
pub fn failure_event(
    task_id: &str,
    round: &str,
    stage: &str,
    reason: &str,
) -> orch_core::EventRecord {
    ledger::event(
        "MechCheckFailed",
        "runtime:orch",
        Some(task_id),
        Some(round),
        serde_json::json!({"stage": stage, "reason": reason}),
    )
}

/// REPORT §0 执行环境自报（r26 修订3）：MODEL=/DEPTH=/CAPTURE= 三行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvDecl {
    pub model: String,
    pub depth: String,
    pub capture: String,
}

/// 从 REPORT 文本提取 §0 段的 MODEL=/DEPTH=/CAPTURE= 三行（顺序不限，值 trim）。
/// 找 `## 0` 或 `## §0` 开头的段；三行缺一/值空 → None；无 §0 段 → None。
pub fn report_env_decl(report_text: &str) -> Option<EnvDecl> {
    let mut in_section = false;
    let mut model: Option<String> = None;
    let mut depth: Option<String> = None;
    let mut capture: Option<String> = None;
    for line in report_text.lines() {
        let trimmed = line.trim();
        // 检测 §0 段起始（容忍 "## 0" 和 "## §0" 两种写法）
        let is_section_start = trimmed.starts_with("## 0") || trimmed.starts_with("## §0");
        if is_section_start {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        // 下一个 ## 段（非 §0）→ 段结束
        if trimmed.starts_with("## ") {
            break;
        }
        // 提取 KEY=VALUE
        if let Some(rest) = trimmed.strip_prefix("MODEL=") {
            model = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("DEPTH=") {
            depth = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("CAPTURE=") {
            capture = Some(rest.trim().to_string());
        }
    }
    if !in_section {
        return None;
    }
    // 三行缺一/值空 → None
    let model = model.filter(|s| !s.is_empty())?;
    let depth = depth.filter(|s| !s.is_empty())?;
    let capture = capture.filter(|s| !s.is_empty())?;
    Some(EnvDecl {
        model,
        depth,
        capture,
    })
}

/// EnvDecl → JSON payload（供 ReportObserved 事件并入账）。
pub fn env_decl_payload(decl: &EnvDecl) -> serde_json::Value {
    serde_json::json!({
        "model": decl.model,
        "depth": decl.depth,
        "capture": decl.capture,
    })
}

/// Immutable result of reconciling the three model-identity layers for one
/// claimed REPORT.  `payload` is embedded in `ReportObserved`; only a strict
/// provider mismatch produces `strict_failure_reason`.
#[derive(Debug, Clone)]
pub struct ReportIdentityAudit {
    pub payload: serde_json::Value,
    pub strict_failure_reason: Option<String>,
    evidence_ready: bool,
    basis: IdentityAuditBasis,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentityAuditBasis {
    source: String,
    tool: Option<String>,
    registry_fingerprint: String,
    wake_event_id: Option<String>,
    receipt_event_id: Option<String>,
}

fn event_payload_str<'a>(event: &'a orch_core::EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(serde_json::Value::as_str)
}

fn event_timestamp_ms(event: &orch_core::EventRecord) -> Option<u64> {
    humantime::parse_rfc3339(&event.ts)
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn identity_payload(identity: &crate::observed::ModelIdentity) -> serde_json::Value {
    serde_json::json!({
        "provider": identity.provider,
        "id": identity.id,
        "qualified": identity.qualified(),
    })
}

fn identity_audit_basis_payload(basis: &IdentityAuditBasis) -> serde_json::Value {
    serde_json::json!({
        "source": basis.source,
        "tool": basis.tool,
        "registryFingerprint": basis.registry_fingerprint,
        "wakeEventId": basis.wake_event_id,
        "receiptEventId": basis.receipt_event_id,
    })
}

fn optional_payload_string(payload: &serde_json::Value, key: &str) -> Option<Option<String>> {
    match payload.get(key) {
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(value)) => Some(Some(value.clone())),
        _ => None,
    }
}

fn recorded_identity_audit_basis(event: &orch_core::EventRecord) -> Option<IdentityAuditBasis> {
    let basis = event
        .payload
        .as_ref()?
        .get("identityReconciliation")?
        .get("evidenceBasis")?;
    let source = basis.get("source")?.as_str()?.to_string();
    let registry_fingerprint = basis.get("registryFingerprint")?.as_str()?.to_string();
    if registry_fingerprint.len() != 64
        || !registry_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(IdentityAuditBasis {
        source,
        tool: optional_payload_string(basis, "tool")?,
        registry_fingerprint,
        wake_event_id: optional_payload_string(basis, "wakeEventId")?,
        receipt_event_id: optional_payload_string(basis, "receiptEventId")?,
    })
}

fn evidence_payload(evidence: &crate::observed::ObservedEvidence) -> serde_json::Value {
    match evidence {
        crate::observed::ObservedEvidence::Found(identity) => serde_json::json!({
            "status": "found",
            "identity": identity_payload(identity),
        }),
        crate::observed::ObservedEvidence::Missing => serde_json::json!({
            "status": "missing",
        }),
        crate::observed::ObservedEvidence::Unreadable(reason) => serde_json::json!({
            "status": "unreadable",
            "reason": reason,
        }),
    }
}

fn outcome_payload(outcome: &crate::observed::ObservedOutcome) -> serde_json::Value {
    match outcome {
        crate::observed::ObservedOutcome::Match => serde_json::json!({"status": "match"}),
        crate::observed::ObservedOutcome::Mismatch { declared, observed } => {
            serde_json::json!({
                "status": "mismatch",
                "declared": identity_payload(declared),
                "observed": identity_payload(observed),
            })
        }
        crate::observed::ObservedOutcome::Missing => serde_json::json!({"status": "missing"}),
        crate::observed::ObservedOutcome::Unreadable { reason } => serde_json::json!({
            "status": "unreadable",
            "reason": reason,
        }),
    }
}

fn matching_wake<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
) -> Option<&'a orch_core::EventRecord> {
    let continuation = format!("implementation:{round}:{task_id}:{attempt_id}:{agent}");
    events.iter().rev().find(|event| {
        event.kind == "WakeIssued"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "attemptId") == Some(attempt_id)
            && event_payload_str(event, "agent") == Some(agent)
            && event_payload_str(event, "continuationId") == Some(&continuation)
    })
}

fn matching_opencode_receipt<'a>(
    events: &'a [orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    wake_id: &str,
) -> Option<&'a orch_core::EventRecord> {
    events.iter().rev().find(|event| {
        event.kind == "AgentEventReceived"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "attemptId") == Some(attempt_id)
            && event_payload_str(event, "agent") == Some(agent)
            && event_payload_str(event, "actionId") == Some(wake_id)
            && event_payload_str(event, "agentEvent") == Some("wake-backend-receipt")
            && event_payload_str(event, "backendState") == Some("accepted")
            && event_payload_str(event, "receiptKind") == Some("opencode")
    })
}

fn managed_wake_terminated(
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    agent: &str,
    wake: &orch_core::EventRecord,
) -> bool {
    let Some(wake_id) = event_payload_str(wake, "wakeId") else {
        return false;
    };
    events.iter().rev().any(|event| {
        event.kind == "ManagedWakeTerminated"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "agent") == Some(agent)
            && event_payload_str(event, "wakeId") == Some(wake_id)
    })
}

fn complete_log_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_inclusive('\n').filter_map(|line| {
        line.strip_suffix('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
    })
}

fn pi_log_is_terminal(text: &str) -> bool {
    complete_log_lines(text).any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|value| {
                value.get("type").and_then(serde_json::Value::as_str) == Some("agent_settled")
            })
    })
}

fn opencode_log_is_terminal(text: &str) -> bool {
    complete_log_lines(text).any(|line| {
        crate::wake::classify_managed_terminal("opencode", line)
            == crate::wake::ManagedTerminal::OpenCodeStop
    })
}

fn observation_log_path(root: &Path, wake: &orch_core::EventRecord) -> Result<PathBuf, String> {
    let raw =
        event_payload_str(wake, "logPath").ok_or_else(|| "WakeIssued lacks logPath".to_string())?;
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("cannot stat wake log {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "wake log is not a regular non-symlink file: {}",
            path.display()
        ));
    }
    let expected = std::fs::canonicalize(root.join("coordination/runtime/logs"))
        .map_err(|error| format!("cannot resolve runtime log directory: {error}"))?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("cannot resolve wake log {}: {error}", path.display()))?;
    if !canonical.starts_with(&expected) {
        return Err(format!(
            "WakeIssued logPath is outside the runtime log directory: {}",
            path.display()
        ));
    }
    Ok(canonical)
}

fn observe_pi(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    agent: &str,
    wake: Option<&orch_core::EventRecord>,
) -> (crate::observed::ObservedEvidence, serde_json::Value, bool) {
    let Some(wake) = wake else {
        return (
            crate::observed::ObservedEvidence::Missing,
            serde_json::json!({"source": "pi-frames"}),
            true,
        );
    };
    let terminated = managed_wake_terminated(events, round, task_id, agent, wake);
    let path = match observation_log_path(root, wake) {
        Ok(path) => path,
        Err(reason) => {
            return (
                crate::observed::ObservedEvidence::Unreadable(reason),
                serde_json::json!({
                    "source": "pi-frames",
                    "managedWakeTerminated": terminated,
                }),
                true,
            );
        }
    };
    let (evidence, terminal) = match std::fs::read_to_string(&path) {
        Ok(text) => (
            crate::observed::extract_pi_response_model(&text),
            pi_log_is_terminal(&text),
        ),
        Err(error) => (
            crate::observed::ObservedEvidence::Unreadable(format!(
                "cannot read pi wake log {}: {error}",
                path.display()
            )),
            false,
        ),
    };
    let ready = terminal
        || terminated
        || matches!(evidence, crate::observed::ObservedEvidence::Unreadable(_));
    (
        evidence,
        serde_json::json!({
            "source": "pi-frames",
            "logPath": path.display().to_string(),
            "terminalObserved": terminal,
            "managedWakeTerminated": terminated,
        }),
        ready,
    )
}

fn observe_opencode(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    wake: Option<&orch_core::EventRecord>,
) -> (crate::observed::ObservedEvidence, serde_json::Value, bool) {
    let Some(wake) = wake else {
        return (
            crate::observed::ObservedEvidence::Missing,
            serde_json::json!({"source": "opencode-db"}),
            true,
        );
    };
    let Some(wake_id) = event_payload_str(wake, "wakeId") else {
        return (
            crate::observed::ObservedEvidence::Unreadable(
                "WakeIssued lacks wakeId for opencode observation".to_string(),
            ),
            serde_json::json!({"source": "opencode-db"}),
            true,
        );
    };
    let terminated = managed_wake_terminated(events, round, task_id, agent, wake);
    let mut scope = serde_json::json!({
        "source": "opencode-db",
        "wakeId": wake_id,
        "managedWakeTerminated": terminated,
    });
    let (terminal, log_error) = match observation_log_path(root, wake) {
        Ok(path) => {
            scope["logPath"] = serde_json::json!(path.display().to_string());
            match std::fs::read_to_string(&path) {
                Ok(text) => (opencode_log_is_terminal(&text), None),
                Err(error) => {
                    let reason =
                        format!("cannot read opencode wake log {}: {error}", path.display());
                    scope["logReadError"] = serde_json::json!(reason);
                    (false, Some(reason))
                }
            }
        }
        Err(reason) => {
            scope["logReadError"] = serde_json::json!(reason);
            (false, Some(reason))
        }
    };
    scope["terminalObserved"] = serde_json::json!(terminal);
    if let Some(reason) = log_error {
        return (
            crate::observed::ObservedEvidence::Unreadable(reason),
            scope,
            true,
        );
    }
    let ready = terminal || terminated;
    if !ready {
        return (crate::observed::ObservedEvidence::Missing, scope, false);
    }
    let receipt = matching_opencode_receipt(events, round, task_id, attempt_id, agent, wake_id);
    let Some(session_id) = receipt.and_then(|event| event_payload_str(event, "observedSessionId"))
    else {
        return (crate::observed::ObservedEvidence::Missing, scope, true);
    };
    scope["sessionId"] = serde_json::json!(session_id);
    let Some(wake_ms) = event_timestamp_ms(wake) else {
        return (
            crate::observed::ObservedEvidence::Unreadable(
                "WakeIssued timestamp is not valid RFC3339".to_string(),
            ),
            scope,
            true,
        );
    };
    let from_ms = wake_ms.saturating_sub(30_000);
    let to_ms: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
        .saturating_add(30_000);
    scope["fromMs"] = serde_json::json!(from_ms);
    scope["toMs"] = serde_json::json!(to_ms);
    let db = match crate::observed::opencode_db_path() {
        Ok(path) => path,
        Err(reason) => {
            return (
                crate::observed::ObservedEvidence::Unreadable(reason),
                scope,
                true,
            );
        }
    };
    let evidence = crate::observed::read_opencode_identity(&db, session_id, from_ms, to_ms);
    scope["databaseUriMode"] = serde_json::json!("ro");
    (evidence, scope, true)
}

fn unconfigured_identity_audit(decl: &EnvDecl, basis: IdentityAuditBasis) -> ReportIdentityAudit {
    let evidence_basis = identity_audit_basis_payload(&basis);
    ReportIdentityAudit {
        payload: serde_json::json!({
            "policy": "advisory",
            "declared": {"model": null, "effort": null, "origin": "unconfigured"},
            "selfReported": {
                "model": decl.model,
                "effort": decl.depth,
                "capture": decl.capture,
                "modelMatchesDeclared": null,
                "effortMatchesDeclared": null,
                "matchesDeclared": null,
            },
            "observed": {"status": "missing", "source": "none"},
            "outcome": {"status": "undeclared"},
            "verificationStatus": "unconfigured",
            "evidenceReady": true,
            "evidenceBasis": evidence_basis,
            "strictMismatch": false,
        }),
        strict_failure_reason: None,
        evidence_ready: true,
        basis,
    }
}

fn validate_identity_observation(tool: &crate::registry::ToolDefinition) -> Result<()> {
    let required = match tool.tool.as_str() {
        "opencode" => ("opencode-db", "strict"),
        "pi" => ("pi-frames", "strict"),
        _ => ("none", "advisory"),
    };
    let configured = (
        tool.observation.source.as_str(),
        tool.observation.policy.as_str(),
    );
    if configured != required {
        bail!(
            "tool {} observation 配置非法: source={} policy={}（要求 source={} policy={}）",
            tool.tool,
            configured.0,
            configured.1,
            required.0,
            required.1,
        );
    }
    Ok(())
}

fn identity_registry_fingerprint(root: &Path, tool: Option<&str>) -> String {
    fn hash_file(hasher: &mut Sha256, label: &str, path: &Path) {
        hasher.update(label.as_bytes());
        hasher.update([0]);
        match std::fs::read(path) {
            Ok(bytes) => {
                hasher.update(b"present\0");
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(bytes);
            }
            Err(error) => {
                hasher.update(b"error\0");
                hasher.update(format!("{:?}", error.kind()).as_bytes());
            }
        }
        hasher.update([0xff]);
    }

    let mut hasher = Sha256::new();
    hash_file(
        &mut hasher,
        "coordination/agents.yaml",
        &root.join("coordination/agents.yaml"),
    );
    if let Some(tool) = tool {
        let relative = format!("coordination/tools/{tool}.yaml");
        hash_file(&mut hasher, &relative, &root.join(&relative));
    }
    hex::encode(hasher.finalize())
}

fn identity_audit_basis(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    source: &str,
) -> IdentityAuditBasis {
    let wake = matching_wake(events, round, task_id, attempt_id, agent);
    let receipt = wake
        .and_then(|event| event_payload_str(event, "wakeId"))
        .filter(|_| source == "opencode-db")
        .and_then(|wake_id| {
            matching_opencode_receipt(events, round, task_id, attempt_id, agent, wake_id)
        });
    IdentityAuditBasis {
        source: source.to_string(),
        tool: wake
            .and_then(|event| event_payload_str(event, "tool"))
            .map(str::to_string),
        registry_fingerprint: identity_registry_fingerprint(
            root,
            wake.and_then(|event| event_payload_str(event, "tool")),
        ),
        wake_event_id: wake.map(|event| event.event_id.clone()),
        receipt_event_id: receipt.map(|event| event.event_id.clone()),
    }
}

pub fn identity_audit_evidence_ready(audit: &ReportIdentityAudit) -> bool {
    audit.evidence_ready
}

pub fn identity_audit_still_current(
    root: &Path,
    audit: &ReportIdentityAudit,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
) -> bool {
    audit.basis
        == identity_audit_basis(
            root,
            events,
            round,
            task_id,
            attempt_id,
            agent,
            &audit.basis.source,
        )
}

pub fn recorded_identity_audit_still_current(
    root: &Path,
    event: &orch_core::EventRecord,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
) -> bool {
    let Some(basis) = recorded_identity_audit_basis(event) else {
        return false;
    };
    basis
        == identity_audit_basis(
            root,
            events,
            round,
            task_id,
            attempt_id,
            agent,
            &basis.source,
        )
}

fn accepted_observed_aliases(
    agent: &str,
    declared: &crate::observed::ModelIdentity,
) -> Vec<crate::observed::ModelIdentity> {
    // B235's frozen task contract is the declaration source for this pair;
    // B233's frozen registry shape has no alias field.  Keep every equivalence
    // agent-scoped, provider-qualified, and conditional on the declared side.
    match (agent, declared.qualified().as_str()) {
        ("executor-zcode", "dcc-dewu-ep/gpt-5-dcc-glm-5-2") => {
            vec![crate::observed::ModelIdentity::parse("dewu-ep/glm-5.2")]
        }
        ("executor-zcode", "dewu-ep/glm-5.2") => vec![crate::observed::ModelIdentity::parse(
            "dcc-dewu-ep/gpt-5-dcc-glm-5-2",
        )],
        _ => Vec::new(),
    }
}

/// Reconcile requested/expected, self-reported, and provider-observed model
/// identities for an immutable REPORT observation.
///
/// Provider reads are local, read-only, and bounded.  The Tier-F caller waits
/// for a strict provider terminal before calling this outside the ledger lock,
/// then revalidates `IdentityAuditBasis` inside the atomic claim.
pub fn audit_report_identity(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    decl: &EnvDecl,
) -> Result<ReportIdentityAudit> {
    let mut after_registry_load = || Ok(());
    audit_report_identity_with_hook(
        root,
        events,
        round,
        task_id,
        attempt_id,
        agent,
        decl,
        &mut after_registry_load,
    )
}

#[allow(clippy::too_many_arguments)]
fn audit_report_identity_with_hook(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    decl: &EnvDecl,
    after_registry_load: &mut dyn FnMut() -> Result<()>,
) -> Result<ReportIdentityAudit> {
    let wake = matching_wake(events, round, task_id, attempt_id, agent);
    let wake_tool = wake.and_then(|event| event_payload_str(event, "tool"));
    let registry_fingerprint_before = identity_registry_fingerprint(root, wake_tool);
    let unconfigured = || -> Result<ReportIdentityAudit> {
        let basis = identity_audit_basis(root, events, round, task_id, attempt_id, agent, "");
        if basis.registry_fingerprint != registry_fingerprint_before {
            bail!("模型勾稽期间 AgentRegistry/ToolDefinition 发生变化，请重试");
        }
        Ok(unconfigured_identity_audit(decl, basis))
    };
    let registry_path = root.join("coordination/agents.yaml");
    if !registry_path.exists() {
        if let Some(wake_tool) = wake_tool {
            bail!("implementation WakeIssued 声明 tool={wake_tool}，但 AgentRegistry 不存在");
        }
        return unconfigured();
    }
    let definitions = crate::registry::load_agent_definitions(root)?;
    after_registry_load()?;
    let Some(definition) = definitions.get(agent) else {
        if let Some(wake_tool) = wake_tool {
            bail!(
                "implementation WakeIssued 声明 tool={wake_tool}，但 AgentRegistry 缺 agent {agent}"
            );
        }
        return unconfigured();
    };
    let tool = match (
        definition.tool.as_deref(),
        definition.tool_definition.as_ref(),
    ) {
        (None, None) if wake_tool.is_none() => return unconfigured(),
        (None, None) => bail!(
            "implementation WakeIssued 声明 tool={}，但 agent {agent} 仍是 legacy 配置",
            wake_tool.unwrap_or_default()
        ),
        (Some(agent_tool), Some(tool)) => {
            let _wake = wake.context("modern agent 缺 exact implementation WakeIssued")?;
            let wake_tool = wake_tool
                .filter(|value| !value.trim().is_empty())
                .context("modern implementation WakeIssued 缺 tool")?;
            if wake_tool != agent_tool || wake_tool != tool.tool {
                bail!(
                    "implementation WakeIssued tool 绑定不一致: wake={wake_tool} agent={agent_tool} definition={}",
                    tool.tool
                );
            }
            tool
        }
        (agent_tool, tool) => bail!(
            "agent {agent} 的 modern tool 配置不完整: agentTool={:?} definition={}",
            agent_tool,
            tool.map(|value| value.tool.as_str()).unwrap_or("missing")
        ),
    };
    validate_identity_observation(tool)?;
    let policy = tool.observation.policy.as_str();
    let source = tool.observation.source.as_str();
    let requested_model = wake.and_then(|event| event_payload_str(event, "requestedModel"));
    let requested_effort = wake.and_then(|event| event_payload_str(event, "requestedEffort"));
    if policy == "strict"
        && requested_model
            .map(str::trim)
            .is_none_or(|model| model.is_empty())
    {
        bail!(
            "strict tool {} 的 implementation WakeIssued 缺 requestedModel",
            tool.tool
        );
    }
    let (declared_model, declared_effort, declared_origin) = if requested_model.is_some() {
        (
            requested_model,
            requested_effort,
            "WakeIssued.requestedModel",
        )
    } else if definition.expected_model.is_some() {
        (
            definition.expected_model.as_deref(),
            definition.expected_effort.as_deref(),
            "AgentRegistry.expectedModel",
        )
    } else {
        (None, None, "missing")
    };
    let basis = identity_audit_basis(root, events, round, task_id, attempt_id, agent, source);
    if basis.registry_fingerprint != registry_fingerprint_before {
        bail!("模型勾稽期间 AgentRegistry/ToolDefinition 发生变化，请重试");
    }
    let (evidence, mut scope, source_ready) = match source {
        "pi-frames" => observe_pi(root, events, round, task_id, agent, wake),
        "opencode-db" => observe_opencode(root, events, round, task_id, attempt_id, agent, wake),
        "none" => (
            crate::observed::ObservedEvidence::Missing,
            serde_json::json!({"source": "none"}),
            true,
        ),
        other => unreachable!("validated observation source {other:?}"),
    };
    scope["evidence"] = evidence_payload(&evidence);
    let evidence_ready = policy != "strict" || source_ready;

    let model_matches =
        declared_model.map(|declared| crate::probe::model_matches(&decl.model, declared));
    let effort_matches = declared_effort.map(|declared| decl.depth.trim() == declared.trim());
    let self_matches = match (model_matches, effort_matches) {
        (None, None) => None,
        (model, effort) => Some(model.unwrap_or(true) && effort.unwrap_or(true)),
    };
    let accepted_aliases = declared_model
        .map(crate::observed::ModelIdentity::parse)
        .map(|declared| accepted_observed_aliases(agent, &declared))
        .unwrap_or_default();
    let (outcome, strict_failure_reason) = match declared_model {
        Some(declared) => {
            let declared = crate::observed::ModelIdentity::parse(declared);
            let outcome =
                crate::observed::reconcile_identity(&declared, &accepted_aliases, &evidence);
            let failure = if policy == "strict" {
                match &outcome {
                    crate::observed::ObservedOutcome::Mismatch { declared, observed } => {
                        Some(format!(
                            "strict model identity mismatch: declared={} observed={}",
                            declared.qualified(),
                            observed.qualified()
                        ))
                    }
                    _ => None,
                }
            } else {
                None
            };
            (Some(outcome), failure)
        }
        None => (None, None),
    };
    let verification_status = if declared_model.is_none() {
        "unconfigured"
    } else if policy != "strict" {
        "advisory-only"
    } else {
        match outcome.as_ref() {
            Some(crate::observed::ObservedOutcome::Match) => "verified",
            Some(crate::observed::ObservedOutcome::Mismatch { .. }) => "mismatch",
            Some(crate::observed::ObservedOutcome::Missing) => "evidence-missing",
            Some(crate::observed::ObservedOutcome::Unreadable { .. }) => "evidence-unreadable",
            None => "unconfigured",
        }
    };
    let outcome_json = outcome
        .as_ref()
        .map(outcome_payload)
        .unwrap_or_else(|| serde_json::json!({"status": "undeclared"}));
    let accepted_aliases_json = accepted_aliases
        .iter()
        .map(identity_payload)
        .collect::<Vec<_>>();
    let evidence_basis = identity_audit_basis_payload(&basis);
    let payload = serde_json::json!({
        "policy": policy,
        "declared": {
            "model": declared_model,
            "effort": declared_effort,
            "origin": declared_origin,
            "acceptedAliases": accepted_aliases_json,
        },
        "selfReported": {
            "model": decl.model,
            "effort": decl.depth,
            "capture": decl.capture,
            "modelMatchesDeclared": model_matches,
            "effortMatchesDeclared": effort_matches,
            "matchesDeclared": self_matches,
        },
        "observed": scope,
        "outcome": outcome_json,
        "verificationStatus": verification_status,
        "evidenceReady": evidence_ready,
        "evidenceBasis": evidence_basis,
        "strictMismatch": strict_failure_reason.is_some(),
    });
    Ok(ReportIdentityAudit {
        payload,
        strict_failure_reason,
        evidence_ready,
        basis,
    })
}

/// Construct the terminal mechanical failure paired atomically after the
/// ReportObserved fact for a strict provider mismatch.
pub fn identity_failure_event(
    task_id: &str,
    round: &str,
    attempt_id: &str,
    attempt_no: Option<usize>,
    audit: &ReportIdentityAudit,
) -> Option<orch_core::EventRecord> {
    let reason = audit.strict_failure_reason.as_deref()?;
    let mut event = failure_event(task_id, round, "model-identity", reason);
    if let Some(payload) = event.payload.as_mut() {
        payload["attemptId"] = serde_json::json!(attempt_id);
        payload["attemptNo"] = serde_json::json!(attempt_no);
        payload["identityReconciliation"] = audit.payload.clone();
    }
    Some(event)
}

/// Fast replay check before provider IO.  The subsequent claim repeats this
/// comparison under lock with `ClaimedEvidence`; this poll-side check only
/// avoids reopening immutable provider evidence on the normal replay path.
pub fn report_identity_observation_already_recorded(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    evidence: &crate::attempt::EvidenceObservation,
) -> bool {
    let Some(control_epoch) = events
        .iter()
        .rev()
        .find(|event| {
            event.task_id.as_deref() == Some(task_id)
                && matches!(
                    event.kind.as_str(),
                    "DispatchIssued" | "NudgeIssued" | "ResumeIssued"
                )
        })
        .map(|event| event.event_id.as_str())
    else {
        return false;
    };
    let canonical_path = evidence.canonical_path.to_string_lossy();
    let matches = events.iter().filter(|event| {
        let payload = event.payload.as_ref();
        event.kind == "ReportObserved"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "actionId") == Some("report-observed")
            && event_payload_str(event, "attemptId") == Some(attempt_id)
            && event_payload_str(event, "evidencePath") == Some(canonical_path.as_ref())
            && event_payload_str(event, "evidenceSha256") == Some(evidence.sha256.as_str())
            && payload
                .and_then(|value| value.get("evidenceLen"))
                .and_then(serde_json::Value::as_u64)
                == Some(evidence.len)
            && event_payload_str(event, "controlEpoch") == Some(control_epoch)
    });
    let matches = matches.collect::<Vec<_>>();
    matches.len() == 1
        && recorded_identity_audit_still_current(
            root, matches[0], events, round, task_id, attempt_id, agent,
        )
}

/// Read the already-recorded immutable reconciliation result before gates run.
/// Missing/unreadable evidence has `strictMismatch=false` and is never blocked.
pub fn recorded_strict_identity_failure(
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    evidence: &crate::attempt::ClaimedEvidence,
) -> Option<String> {
    let event = events.iter().rev().find(|event| {
        let payload = event.payload.as_ref();
        event.kind == "ReportObserved"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "actionId") == Some("report-observed")
            && event_payload_str(event, "attemptId") == Some(attempt_id)
            && event_payload_str(event, "evidencePath") == Some(evidence.canonical_path.as_str())
            && event_payload_str(event, "evidenceSha256") == Some(evidence.sha256.as_str())
            && payload
                .and_then(|value| value.get("evidenceLen"))
                .and_then(serde_json::Value::as_u64)
                == Some(evidence.len)
            && event_payload_str(event, "controlEpoch") == Some(evidence.control_epoch.as_str())
    })?;
    let identity = event.payload.as_ref()?.get("identityReconciliation")?;
    if identity
        .get("strictMismatch")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    identity
        .get("outcome")
        .and_then(|outcome| outcome.get("status"))
        .and_then(serde_json::Value::as_str)
        .filter(|status| *status == "mismatch")?;
    Some(
        identity
            .get("outcome")
            .map(|outcome| format!("strict model identity mismatch: {outcome}"))
            .unwrap_or_else(|| "strict model identity mismatch".to_string()),
    )
}

pub fn ensure_recorded_identity_allows_collect(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    evidence: &crate::attempt::ClaimedEvidence,
) -> Result<()> {
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = orch_core::read_ledger(&ledger_path)
        .with_context(|| format!("读取模型勾稽账本失败: {}", ledger_path.display()))?;
    crate::attempt::reject_bad_lines(&read)?;
    ensure_recorded_identity_allows_collect_in_events(
        root,
        &read.events,
        round,
        task_id,
        attempt_id,
        agent,
        evidence,
    )
}

/// Ledger-lock variant used by collect's durable fold.  Keeping the check on
/// the exact event slice that is about to be folded prevents a terminal
/// Completed/Executed replay from racing past a stale or forged observation.
pub(crate) fn ensure_recorded_identity_allows_collect_in_events(
    root: &Path,
    events: &[orch_core::EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    agent: &str,
    evidence: &crate::attempt::ClaimedEvidence,
) -> Result<()> {
    let matching = events.iter().filter(|event| {
        let payload = event.payload.as_ref();
        event.kind == "ReportObserved"
            && event.actor == "runtime:orch"
            && event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "actionId") == Some("report-observed")
            && event_payload_str(event, "attemptId") == Some(attempt_id)
            && event_payload_str(event, "evidencePath") == Some(evidence.canonical_path.as_str())
            && event_payload_str(event, "evidenceSha256") == Some(evidence.sha256.as_str())
            && payload
                .and_then(|value| value.get("evidenceLen"))
                .and_then(serde_json::Value::as_u64)
                == Some(evidence.len)
            && event_payload_str(event, "controlEpoch") == Some(evidence.control_epoch.as_str())
    });
    let matching = matching.collect::<Vec<_>>();
    if matching.len() != 1 {
        bail!(
            "REPORT collect 要求唯一带模型勾稽的 ReportObserved，实得 {}",
            matching.len()
        );
    }
    if !recorded_identity_audit_still_current(
        root,
        matching[0],
        events,
        round,
        task_id,
        attempt_id,
        agent,
    ) {
        bail!("ReportObserved 模型勾稽锚点缺失或过期，拒绝 collect");
    }
    if let Some(reason) =
        recorded_strict_identity_failure(events, round, task_id, attempt_id, evidence)
    {
        bail!(reason);
    }
    Ok(())
}

/// B108：临时路径卫生机检发现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempPathFinding {
    /// 命中的规则标识（稳定、可机读）。
    pub rule: &'static str,
    /// 触发判定的源码证据片段。
    pub evidence: String,
}

/// 对真实 Rust 源码树执行临时路径卫生检查时产生的可定位发现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTempPathFinding {
    pub path: std::path::PathBuf,
    pub line: usize,
    pub evidence: String,
}

/// 真实源码树扫描报告。覆盖面和发现均完整返回，调用方可审计而非只看计数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTempPathHygieneReport {
    pub scanned_files: usize,
    pub scanned_paths: Vec<std::path::PathBuf>,
    pub findings: Vec<RepoTempPathFinding>,
}

/// 临时路径卫生扫描（零模型文本判定）：
/// 对「pid + nanos/时间戳 但无计数器」的临时名构造报 Finding；
/// 统一 helper `unique_scratch_name`（pid + AtomicU64 计数器）不误报。
///
/// 判定逻辑：
/// - 代码引用 `unique_scratch_name` → 合规，直接放行；
/// - 未构造临时路径（无 `temp_dir()`/`/tmp`）→ 不干预；
/// - 同时出现 pid 与时间源标记、且无任何计数器标记 → 报 Finding
///   （时钟粒度+并发下可撞名，且无法审计序号）。
pub fn temp_path_hygiene(code: &str) -> Vec<TempPathFinding> {
    let mut findings = Vec::new();
    if code.contains("unique_scratch_name") {
        return findings;
    }
    let builds_temp_path = code.contains("temp_dir()") || code.contains("/tmp");
    if !builds_temp_path {
        return findings;
    }
    const TIME_MARKERS: [&str; 6] = [
        "as_nanos",
        "nanos",
        "SystemTime",
        "UNIX_EPOCH",
        "timestamp",
        "epoch",
    ];
    const COUNTER_MARKERS: [&str; 3] = ["AtomicU64", "AtomicUsize", "fetch_add"];
    let has_pid = code.contains("process::id()");
    let time_hit = TIME_MARKERS.iter().find(|m| code.contains(**m));
    let has_counter = COUNTER_MARKERS.iter().any(|m| code.contains(m));
    if has_pid && !has_counter {
        if let Some(marker) = time_hit {
            let evidence = code
                .lines()
                .find(|line| line.contains("process::id()") || line.contains(marker))
                .unwrap_or(code)
                .trim()
                .chars()
                .take(120)
                .collect();
            findings.push(TempPathFinding {
                rule: "pid-plus-time-without-counter",
                evidence,
            });
        }
    }
    findings
}

/// 遍历 `crates_root` 下每个 crate 的 `src/` 与 `tests/` Rust 文件，并按函数体运行
/// 临时路径卫生判据。覆盖面来自目录遍历；注释和字符串会先被遮蔽，因此描述坏形状的
/// 文档、测试样本字符串不会被当成真正构造路径的代码。
pub fn scan_repo_temp_path_hygiene(crates_root: &Path) -> Result<RepoTempPathHygieneReport> {
    let mut files = Vec::new();
    if crates_root.is_dir() {
        for crate_entry in std::fs::read_dir(crates_root)
            .with_context(|| format!("读取 crates 根失败: {}", crates_root.display()))?
        {
            let crate_path = crate_entry?.path();
            if !crate_path.is_dir() {
                continue;
            }
            for source_dir in [crate_path.join("src"), crate_path.join("tests")] {
                collect_rust_files(&source_dir, &mut files)?;
            }
        }
    }
    files.sort();

    let mut scanned_paths = Vec::with_capacity(files.len());
    let mut findings = Vec::new();
    for absolute_path in files {
        let relative_path = absolute_path
            .strip_prefix(crates_root)
            .unwrap_or(&absolute_path)
            .to_path_buf();
        let source = std::fs::read_to_string(&absolute_path)
            .with_context(|| format!("读取 Rust 源码失败: {}", absolute_path.display()))?;
        let sanitized = mask_rust_comments_and_strings(&source);
        for (start, end) in rust_function_ranges(&sanitized) {
            let function = &sanitized[start..end];
            if temp_path_hygiene(function).is_empty() {
                continue;
            }
            let evidence_offset = function
                .find("process::id()")
                .or_else(|| function.find("as_nanos"))
                .unwrap_or(0);
            let absolute_offset = start + evidence_offset;
            let line = sanitized[..absolute_offset]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            let evidence = source
                .lines()
                .nth(line.saturating_sub(1))
                .unwrap_or_default()
                .trim()
                .chars()
                .take(160)
                .collect::<String>();
            findings.push(RepoTempPathFinding {
                path: relative_path.clone(),
                line,
                evidence,
            });
        }
        scanned_paths.push(relative_path);
    }

    Ok(RepoTempPathHygieneReport {
        scanned_files: scanned_paths.len(),
        scanned_paths,
        findings,
    })
}

fn collect_rust_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("遍历源码目录失败: {}", dir.display()))
        }
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_rust_files(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// 用空格遮蔽注释与字符串，保留字节长度和换行位置，便于后续报告源码行号。
fn mask_rust_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let start = i;
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            mask_non_newlines(&mut masked[start..i]);
        } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            mask_non_newlines(&mut masked[start..i]);
        } else if let Some((hashes, quote)) = raw_string_start(bytes, i) {
            let start = i;
            i = quote + 1;
            while i < bytes.len() {
                if bytes[i] == b'"'
                    && bytes.get(i + 1..i + 1 + hashes) == Some(&bytes[quote - hashes..quote])
                {
                    i += 1 + hashes;
                    break;
                }
                i += 1;
            }
            mask_non_newlines(&mut masked[start..i]);
        } else if bytes[i] == b'"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            mask_non_newlines(&mut masked[start..i]);
        } else {
            i += 1;
        }
    }
    String::from_utf8(masked).expect("遮蔽只写 ASCII 空格，UTF-8 应保持有效")
}

fn mask_non_newlines(bytes: &mut [u8]) {
    for byte in bytes {
        if *byte != b'\n' && *byte != b'\r' {
            *byte = b' ';
        }
    }
}

fn raw_string_start(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    if bytes.get(start) != Some(&b'r') {
        return None;
    }
    let mut cursor = start + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some((cursor - start - 1, cursor))
}

fn rust_function_ranges(code: &str) -> Vec<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while cursor + 1 < bytes.len() {
        let is_fn = bytes[cursor] == b'f'
            && bytes[cursor + 1] == b'n'
            && (cursor == 0 || !is_ident_byte(bytes[cursor - 1]))
            && bytes
                .get(cursor + 2)
                .is_none_or(|byte| !is_ident_byte(*byte));
        if !is_fn {
            cursor += 1;
            continue;
        }
        let Some(open_rel) = code[cursor + 2..].find('{') else {
            break;
        };
        let open = cursor + 2 + open_rel;
        let mut depth = 1usize;
        let mut end = open + 1;
        while end < bytes.len() && depth > 0 {
            match bytes[end] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        if depth == 0 {
            ranges.push((cursor, end));
            cursor = end;
        } else {
            break;
        }
    }
    ranges
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn branch_has_upstream(root: &Path, branch: &str) -> Result<bool> {
    let absolute_root = root
        .canonicalize()
        .with_context(|| format!("解析仓库绝对路径失败: {}", root.display()))?;
    let upstream = format!("{branch}@{{upstream}}");
    let status = Command::new("git")
        .arg("-C")
        .arg(&absolute_root)
        .args(["rev-parse", "--verify", "--quiet", &upstream])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("检查分支 upstream 失败: git -C {}", absolute_root.display()))?;
    Ok(status.success())
}

pub fn check(
    root: &Path,
    c: &card::Card,
    branch: &str,
    report_rel: &str,
    expected_base: Option<&str>,
) -> Result<MechOutcome> {
    crate::oracle::validate_seed_paths(root, c)?;
    let mut notes = Vec::new();
    let mb = gitx::merge_base(root, "main", branch)?;
    if let Some(exp) = expected_base {
        if mb != exp {
            // A task can be dispatched before a prerequisite is Recorded.  A later
            // attempt may then merge the trusted, advanced main into its branch.
            // Accept only that forward ancestry relation; rewinds and sideways
            // moves remain fail-closed.  Domain and commit-shape checks below still
            // use the actual E4 merge-base.
            let forward = gitx::is_ancestor(root, exp, &mb)?;
            if !merge_base_matches_policy(exp, &mb, forward) {
                bail!(
                    "merge-base {} ≠ 派发基线 {}（现场被移动？）",
                    gitx::short(&mb),
                    gitx::short(exp)
                );
            }
            notes.push(format!(
                "派发基线前移 ✅ {} → {}（仅吸收 main 祖先链）",
                gitx::short(exp),
                gitx::short(&mb)
            ));
        }
    }
    // 域检（E4：merge-base 为基）；绑定缺失/损坏时按旧仓兼容语义降级为空 protected。
    let changed = gitx::diff_names(root, &mb, branch)?;
    let project_binding = binding::load(root).ok();
    let protected = project_binding
        .as_ref()
        .map(|value| value.scope.protected_paths.as_slice())
        .unwrap_or_default();
    path_guard(
        &changed,
        &c.meta.write_set,
        &c.meta.frozen_paths,
        protected,
        report_rel,
    )?;
    notes.push(format!("文件域 ✅ 改动 {} 个文件全部在域内", changed.len()));

    if let Some(project_binding) = &project_binding {
        if project_binding.git.push_policy == "forbidden" {
            if branch_has_upstream(root, branch)? {
                bail!("分支已配置 upstream，pushPolicy=forbidden 违约: {branch}");
            }
            notes.push("pushPolicy=forbidden ✅ 任务分支无 upstream".into());
        }
    }

    // 提交形状 + 种子字节
    let commits = gitx::commits_after(root, &mb, branch)?;
    if commits.is_empty() {
        bail!("分支无提交");
    }
    let mut seed_digests = Vec::new();
    if !c.meta.seeds.is_empty() {
        let first_files = gitx::commit_files(root, &commits[0])?;
        let seed_targets: Vec<&String> = c.meta.seeds.iter().map(|s| &s.target).collect();
        if !(first_files
            .iter()
            .all(|f| seed_targets.iter().any(|t| *t == f))
            && !first_files.is_empty())
        {
            bail!(
                "首 commit 应只含种子搬运，实际: {:?}（subject: {}）",
                first_files,
                gitx::commit_subject(root, &commits[0])?
            );
        }
        notes.push(format!(
            "提交形状 ✅ 首 commit 仅种子（共 {} commits）",
            commits.len()
        ));
        for s in &c.meta.seeds {
            let branch_bytes = gitx::show_bytes(root, branch, &s.target)?;
            let src_bytes = crate::oracle::read_bound_seed_bytes(root, &s.src)?;
            if branch_bytes != src_bytes {
                bail!("种子字节不一致: {} vs {}", s.src, s.target);
            }
            let digest = hex::encode(Sha256::digest(&branch_bytes));
            if let Some(expected) = &s.sha256 {
                if &digest != expected {
                    bail!(
                        "种子 SHA 不符: {} 期望 {} 实际 {}",
                        s.target,
                        expected,
                        digest
                    );
                }
            }
            seed_digests.push((s.target.clone(), digest));
        }
        notes.push("种子字节 ✅ cmp+SHA-256 一致".into());
    }
    Ok(MechOutcome {
        merge_base: mb,
        notes,
        seed_digests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_fixture(policy: &str, frames: &str) -> (PathBuf, orch_core::EventRecord, EnvDecl) {
        let root = crate::util::test_scratch_dir(&format!("b235-identity-{policy}"));
        std::fs::create_dir_all(root.join("coordination/tools")).unwrap();
        std::fs::create_dir_all(root.join("coordination/runtime/logs")).unwrap();
        std::fs::write(
            root.join("coordination/tools/pi.yaml"),
            format!(
                r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: pi
modelSource: env
startupSpacingMs: 0
maxConcurrent: 2
env:
  model: ORCH_PI_MODEL
  effort: ORCH_PI_EFFORT
launch:
  argv: ["sh", "wake-pi.sh", "{{message}}"]
observation: {{source: pi-frames, policy: {policy}}}
"#
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("coordination/agents.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-pi:
    tool: pi
    model: deepseek/deepseek-chat
    effort: max
    injectable: true
    sessionId: fresh
"#,
        )
        .unwrap();
        let log_path = root.join("coordination/runtime/logs/wake-pi-attempt.jsonl");
        std::fs::write(&log_path, frames).unwrap();
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": "executor-pi",
                "continuationId": "implementation:r66:B235:B235-A0002:executor-pi",
                "wakeId": "wake-pi-one",
                "tool": "pi",
                "logPath": log_path,
                "requestedModel": "deepseek/deepseek-chat",
                "requestedEffort": "max",
            }),
        );
        let decl = EnvDecl {
            model: "deepseek/deepseek-chat".to_string(),
            depth: "max".to_string(),
            capture: "provider frame".to_string(),
        };
        (root, wake, decl)
    }

    fn advisory_identity_fixture(
        tool_name: &str,
        agent: &str,
        expected_model: &str,
        reported_model: &str,
    ) -> (PathBuf, orch_core::EventRecord, EnvDecl) {
        let root = crate::util::test_scratch_dir(&format!("b235-identity-advisory-{tool_name}"));
        std::fs::create_dir_all(root.join("coordination/tools")).unwrap();
        std::fs::write(
            root.join(format!("coordination/tools/{tool_name}.yaml")),
            format!(
                r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: {tool_name}
modelSource: external-config
startupSpacingMs: 0
maxConcurrent: 2
launch:
  argv: ["{tool_name}", "{{message}}"]
observation: {{source: none, policy: advisory}}
"#
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("coordination/agents.yaml"),
            format!(
                r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  {agent}:
    tool: {tool_name}
    expectedModel: {expected_model}
    expectedEffort: xhigh
    injectable: true
    sessionId: fresh
"#
            ),
        )
        .unwrap();
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": agent,
                "continuationId": format!("implementation:r66:B235:B235-A0002:{agent}"),
                "wakeId": format!("wake-{tool_name}-one"),
                "tool": tool_name,
            }),
        );
        let decl = EnvDecl {
            model: reported_model.to_string(),
            depth: "xhigh".to_string(),
            capture: "external config".to_string(),
        };
        (root, wake, decl)
    }

    #[test]
    fn merge_base_policy_accepts_equal_or_forward_only() {
        assert!(merge_base_matches_policy("base", "base", false));
        assert!(merge_base_matches_policy("base", "recorded-main", true));
        assert!(!merge_base_matches_policy("base", "sideways", false));
    }

    #[test]
    fn failure_event_kind_actor_task_round_shape() {
        let ev = failure_event("B19", "r13", "domain", "文件域越界: x.rs");
        assert_eq!(ev.kind, "MechCheckFailed");
        assert_eq!(ev.actor, "runtime:orch");
        assert_eq!(ev.task_id.as_deref(), Some("B19"));
        assert_eq!(ev.round.as_deref(), Some("r13"));
    }

    #[test]
    fn failure_event_payload_has_stage_and_reason() {
        let ev = failure_event("B19", "r13", "seed-bytes", "字节不一致");
        let p = ev.payload.as_ref().expect("payload present");
        assert_eq!(p["stage"], "seed-bytes");
        assert_eq!(p["reason"], "字节不一致");
    }

    #[test]
    fn failure_event_id_is_26_char_ulid() {
        let ev = failure_event("B19", "r13", "shape", "无提交");
        assert_eq!(ev.event_id.len(), 26);
        assert!(ev.event_id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn path_guard_domain_violation_bails() {
        let err = path_guard(
            &["design/x.md".to_string()],
            &["orch/src/x.rs".to_string()],
            &[],
            &["design/**".to_string()],
            "coordination/rounds/r13/reports/B19-REPORT.md",
        )
        .unwrap_err();
        assert!(err.to_string().contains("protected"));
    }

    #[test]
    fn report_committed_rejects_empty_and_whitespace_only_trees() {
        assert!(!report_committed(
            "",
            "coordination/rounds/r16/reports/B27-REPORT.md"
        ));
        assert!(!report_committed(
            "\n  \r\n\t\n",
            "coordination/rounds/r16/reports/B27-REPORT.md"
        ));
    }

    #[test]
    fn report_committed_accepts_unicode_and_spaces_as_exact_path() {
        let report = "coordination/轮次 r16/reports/执行者 报告.md";
        let tree = format!("README.md\n{report}\r\nsrc/lib.rs\n");
        assert!(report_committed(&tree, report));
    }

    #[test]
    fn report_committed_does_not_trim_or_match_path_substrings() {
        let report = "coordination/rounds/r16/reports/B27-REPORT.md";
        let tree = format!("{report}.bak\nprefix/{report}\n {report}\n");
        assert!(!report_committed(&tree, report));
    }

    #[test]
    fn report_env_decl_section_at_end_without_successor_parses() {
        // §0 段在文末、无后继 ## 段 → 仍解析成功
        let report = "## §0 执行环境自报\nMODEL=glm-5.2\nDEPTH=standard\nCAPTURE=grep x\n";
        let decl = report_env_decl(report).expect("文末无后继段应解析");
        assert_eq!(decl.model, "glm-5.2");
        assert_eq!(decl.depth, "standard");
        assert_eq!(decl.capture, "grep x");
    }

    #[test]
    fn report_env_decl_accepts_both_heading_styles() {
        // "## 0" 和 "## §0" 两种写法等价
        let a = report_env_decl("## 0 执行环境自报\nMODEL=a\nDEPTH=b\nCAPTURE=c\n");
        let b = report_env_decl("## §0 执行环境自报\nMODEL=a\nDEPTH=b\nCAPTURE=c\n");
        assert_eq!(a, b);
        assert!(a.is_some());
    }

    #[test]
    fn strict_provider_mismatch_records_report_then_mech_failure() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat","responseModel":"deepseek-v4-flash"}}
{"type":"agent_settled"}
"#;
        let (root, wake, decl) = identity_fixture("strict", frames);
        let audit = audit_report_identity(
            &root,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["verificationStatus"], "mismatch");
        assert_eq!(audit.payload["selfReported"]["matchesDeclared"], true);
        assert!(audit.strict_failure_reason.is_some());

        let claimed = crate::attempt::ClaimedEvidence {
            canonical_path: "/tmp/B235-REPORT.md".to_string(),
            sha256: "report-sha".to_string(),
            len: 42,
            bytes: b"immutable report".to_vec(),
            control_epoch: "dispatch-event-id".to_string(),
        };
        let mut report = ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "actionId": "report-observed",
                "attemptId": "B235-A0002",
                "evidencePath": claimed.canonical_path.clone(),
                "evidenceSha256": claimed.sha256.clone(),
                "evidenceLen": claimed.len,
                "controlEpoch": claimed.control_epoch.clone(),
                "identityReconciliation": audit.payload,
            }),
        );
        let failure = identity_failure_event("B235", "r66", "B235-A0002", Some(2), &audit)
            .expect("strict mismatch must create a failure");
        let ordered = vec![report.clone(), failure];
        assert_eq!(ordered[0].kind, "ReportObserved");
        assert_eq!(ordered[1].kind, "MechCheckFailed");
        assert!(
            recorded_strict_identity_failure(&ordered, "r66", "B235", "B235-A0002", &claimed,)
                .is_some()
        );
        assert_eq!(
            orch_core::fold(&ordered).tasks["B235"].state,
            Some(orch_core::TaskState::ChangesRequested)
        );

        let mut newer_report = report.clone();
        newer_report.payload.as_mut().unwrap()["controlEpoch"] =
            serde_json::json!("resume-event-id");
        newer_report.payload.as_mut().unwrap()["identityReconciliation"]["strictMismatch"] =
            serde_json::json!(false);
        let mut newer_claim = claimed.clone();
        newer_claim.control_epoch = "resume-event-id".to_string();
        assert!(recorded_strict_identity_failure(
            &[ordered[0].clone(), newer_report],
            "r66",
            "B235",
            "B235-A0002",
            &newer_claim,
        )
        .is_none());

        report.payload.as_mut().unwrap()["identityReconciliation"]["strictMismatch"] =
            serde_json::json!(false);
        assert!(
            recorded_strict_identity_failure(&[report], "r66", "B235", "B235-A0002", &claimed,)
                .is_none()
        );
    }

    #[test]
    fn production_aliases_are_agent_scoped_and_provider_qualified() {
        let local = crate::observed::ModelIdentity::parse("dcc-dewu-ep/gpt-5-dcc-glm-5-2");
        let aliases = accepted_observed_aliases("executor-zcode", &local);
        assert_eq!(
            aliases,
            vec![crate::observed::ModelIdentity::parse("dewu-ep/glm-5.2")]
        );
        assert!(accepted_observed_aliases("executor-opencode", &local).is_empty());
        assert!(accepted_observed_aliases(
            "executor-zcode",
            &crate::observed::ModelIdentity::parse("other/model"),
        )
        .is_empty());
    }

    #[test]
    fn strict_missing_evidence_is_unverified_but_not_failed() {
        let frames = r#"{"type":"message_start","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat"}}
{"type":"agent_settled"}
"#;
        let (root, wake, decl) = identity_fixture("strict", frames);
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["verificationStatus"], "evidence-missing");
        assert_eq!(audit.payload["outcome"]["status"], "missing");
        assert!(identity_audit_evidence_ready(&audit));
        assert!(audit.strict_failure_reason.is_none());
        assert!(identity_failure_event("B235", "r66", "B235-A0002", Some(2), &audit).is_none());
    }

    #[test]
    fn strict_pi_missing_before_terminal_is_not_ready_to_freeze() {
        let frames = r#"{"type":"message_start","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat"}}
"#;
        let (root, wake, decl) = identity_fixture("strict", frames);
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["outcome"]["status"], "missing");
        assert!(!identity_audit_evidence_ready(&audit));
        assert!(audit.strict_failure_reason.is_none());
    }

    #[test]
    fn advisory_tool_is_never_labeled_verified_or_failed() {
        let (root, wake, decl) = advisory_identity_fixture(
            "zcode",
            "executor-zcode",
            "dcc-dewu-ep/gpt-5-dcc-glm-5-2",
            "dcc-dewu-ep/gpt-5-dcc-glm-5-2",
        );
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-zcode",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["outcome"]["status"], "missing");
        assert_eq!(audit.payload["verificationStatus"], "advisory-only");
        assert_ne!(audit.payload["verificationStatus"], "verified");
        assert!(audit.strict_failure_reason.is_none());
    }

    #[test]
    fn codex_self_report_conflict_remains_advisory_only() {
        let (root, wake, decl) = advisory_identity_fixture(
            "codex",
            "executor-desktop",
            "openai/gpt-5.6-sol",
            "openai/a-different-self-report",
        );
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-desktop",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["selfReported"]["matchesDeclared"], false);
        assert_eq!(audit.payload["verificationStatus"], "advisory-only");
        assert_ne!(audit.payload["verificationStatus"], "verified");
        assert!(audit.strict_failure_reason.is_none());
    }

    #[test]
    fn strict_tool_policy_typo_fails_loud() {
        let (root, wake, decl) = identity_fixture("strcit", "");
        let error = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .expect_err("strict provider policy typo must not become advisory");
        assert!(error.to_string().contains("observation 配置非法"));
    }

    #[test]
    fn strict_wake_without_requested_model_fails_loud() {
        let (root, mut wake, decl) = identity_fixture("strict", "");
        wake.payload
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("requestedModel");
        let error = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .expect_err("strict declaration must come from the exact wake");
        assert!(error.to_string().contains("缺 requestedModel"));
    }

    #[test]
    fn strict_match_uses_observation_even_when_self_report_conflicts() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat","responseModel":"deepseek-chat"}}
{"type":"agent_settled"}
"#;
        let (root, wake, mut decl) = identity_fixture("strict", frames);
        decl.model = "deepseek/self-report-wrong".to_string();
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["selfReported"]["matchesDeclared"], false);
        assert_eq!(audit.payload["outcome"]["status"], "match");
        assert_eq!(audit.payload["verificationStatus"], "verified");
        assert!(identity_audit_evidence_ready(&audit));
        assert!(audit.strict_failure_reason.is_none());
    }

    #[test]
    fn self_report_effort_mismatch_is_recorded_separately() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"deepseek","responseModel":"deepseek-chat"}}
{"type":"agent_settled"}
"#;
        let (root, wake, mut decl) = identity_fixture("strict", frames);
        decl.depth = "low".to_string();
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["selfReported"]["modelMatchesDeclared"], true);
        assert_eq!(
            audit.payload["selfReported"]["effortMatchesDeclared"],
            false
        );
        assert_eq!(audit.payload["selfReported"]["matchesDeclared"], false);
        assert_eq!(audit.payload["verificationStatus"], "verified");
    }

    #[test]
    fn modern_registry_cannot_drift_from_the_frozen_wake_tool() {
        let (root, mut wake, decl) = identity_fixture("strict", "");
        wake.payload.as_mut().unwrap()["tool"] = serde_json::json!("zcode");
        let error = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .expect_err("frozen wake tool must bind the observation policy");
        assert!(error.to_string().contains("tool 绑定不一致"));
    }

    #[test]
    fn registry_change_between_parse_and_basis_is_retried() {
        let (root, wake, decl) = identity_fixture("strict", "");
        let registry_path = root.join("coordination/agents.yaml");
        let mut mutate_after_parse = || -> Result<()> {
            let changed = std::fs::read_to_string(&registry_path)?
                .replace("deepseek/deepseek-chat", "deepseek/deepseek-other");
            std::fs::write(&registry_path, changed)?;
            Ok(())
        };
        let error = audit_report_identity_with_hook(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
            &mut mutate_after_parse,
        )
        .expect_err("parsed registry bytes must match the recorded basis fingerprint");
        assert!(error.to_string().contains("发生变化"));
    }

    #[test]
    fn unconfigured_audit_is_invalidated_by_registry_migration() {
        let root = crate::util::test_scratch_dir("b235-unconfigured-registry-drift");
        std::fs::create_dir_all(root.join("coordination")).unwrap();
        std::fs::write(
            root.join("coordination/agents.yaml"),
            r#"agents:
  executor-pi:
    injectable: true
    sessionId: fresh
    wake: {argv: ["pi", "{message}"]}
"#,
        )
        .unwrap();
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": "executor-pi",
                "continuationId": "implementation:r66:B235:B235-A0002:executor-pi",
                "wakeId": "legacy-wake",
            }),
        );
        let decl = EnvDecl {
            model: "deepseek/deepseek-chat".to_string(),
            depth: "max".to_string(),
            capture: "legacy self report".to_string(),
        };
        let audit = audit_report_identity(
            &root,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["verificationStatus"], "unconfigured");
        let report = ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({"identityReconciliation": audit.payload.clone()}),
        );
        assert!(identity_audit_still_current(
            &root,
            &audit,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));
        assert!(recorded_identity_audit_still_current(
            &root,
            &report,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));

        std::fs::write(
            root.join("coordination/agents.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-pi:
    tool: pi
    model: deepseek/deepseek-chat
    effort: max
    injectable: true
    sessionId: fresh
"#,
        )
        .unwrap();
        assert!(!identity_audit_still_current(
            &root,
            &audit,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));
        assert!(!recorded_identity_audit_still_current(
            &root,
            &report,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));
    }

    #[test]
    fn strict_unreadable_evidence_is_ready_and_does_not_fail_the_report() {
        let frames = r#"{"type":"turn_end","message":{"role":"assistant","responseModel":"deepseek-chat"}}
"#;
        let (root, wake, decl) = identity_fixture("strict", frames);
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["outcome"]["status"], "unreadable");
        assert_eq!(audit.payload["verificationStatus"], "evidence-unreadable");
        assert!(identity_audit_evidence_ready(&audit));
        assert!(audit.strict_failure_reason.is_none());
    }

    #[test]
    fn strict_opencode_unreadable_log_is_ready_and_nonblocking() {
        let root = crate::util::test_scratch_dir("b235-opencode-unreadable-log");
        std::fs::create_dir_all(root.join("coordination/tools")).unwrap();
        std::fs::write(
            root.join("coordination/tools/opencode.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: opencode
modelSource: argv
startupSpacingMs: 0
maxConcurrent: 2
launch:
  argv: ["opencode", "run", "{message}", "--model", "{model}", "--variant", "{effort}"]
observation: {source: opencode-db, policy: strict}
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("coordination/agents.yaml"),
            r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-opencode:
    tool: opencode
    model: one-dewu-opencode/glm-5.2
    effort: high
    injectable: true
    sessionId: fresh
"#,
        )
        .unwrap();
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": "executor-opencode",
                "continuationId": "implementation:r66:B235:B235-A0002:executor-opencode",
                "wakeId": "wake-opencode-unreadable",
                "tool": "opencode",
                "requestedModel": "one-dewu-opencode/glm-5.2",
                "requestedEffort": "high",
            }),
        );
        let decl = EnvDecl {
            model: "one-dewu-opencode/glm-5.2".to_string(),
            depth: "high".to_string(),
            capture: "opencode db".to_string(),
        };
        let audit = audit_report_identity(
            &root,
            &[wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-opencode",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["outcome"]["status"], "unreadable");
        assert_eq!(audit.payload["verificationStatus"], "evidence-unreadable");
        assert!(identity_audit_evidence_ready(&audit));
        assert!(audit.strict_failure_reason.is_none());
        assert!(identity_failure_event("B235", "r66", "B235-A0002", Some(2), &audit).is_none());
    }

    #[test]
    fn reviewer_wake_cannot_replace_the_exact_implementation_declaration() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"deepseek","responseModel":"deepseek-chat"}}
{"type":"agent_settled"}
"#;
        let (root, implementation, decl) = identity_fixture("strict", frames);
        let review = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": "executor-pi",
                "continuationId": "review:r66:B235:B235-A0002:nongate:executor-pi",
                "wakeId": "review-wake",
                "requestedModel": "deepseek/reviewer-model",
            }),
        );
        let audit = audit_report_identity(
            &root,
            &[implementation, review],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        assert_eq!(audit.payload["declared"]["model"], "deepseek/deepseek-chat");
        assert_eq!(audit.payload["verificationStatus"], "verified");
    }

    #[test]
    fn terminal_detection_requires_complete_exact_records() {
        assert!(!pi_log_is_terminal(r#"{"type":"agent_settled"}"#));
        assert!(pi_log_is_terminal("{\"type\":\"agent_settled\"}\n"));
        assert!(!opencode_log_is_terminal(
            "{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"tool-calls\"}}\n"
        ));
        assert!(opencode_log_is_terminal(
            "{\"type\":\"step_finish\",\"part\":{\"type\":\"step-finish\",\"reason\":\"stop\"}}\n"
        ));
    }

    #[test]
    fn late_opencode_receipt_changes_the_audit_basis() {
        let root = crate::util::test_scratch_dir("b235-late-opencode-receipt-basis");
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": "executor-opencode",
                "continuationId": "implementation:r66:B235:B235-A0002:executor-opencode",
                "wakeId": "wake-opencode-one",
            }),
        );
        let before = identity_audit_basis(
            &root,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-opencode",
            "opencode-db",
        );
        let receipt = ledger::event(
            "AgentEventReceived",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "attemptId": "B235-A0002",
                "agent": "executor-opencode",
                "actionId": "wake-opencode-one",
                "agentEvent": "wake-backend-receipt",
                "backendState": "accepted",
                "receiptKind": "opencode",
                "observedSessionId": "ses_exact",
            }),
        );
        let after = identity_audit_basis(
            &root,
            &[wake, receipt],
            "r66",
            "B235",
            "B235-A0002",
            "executor-opencode",
            "opencode-db",
        );
        assert_ne!(before, after);
        assert!(before.receipt_event_id.is_none());
        assert!(after.receipt_event_id.is_some());
    }

    #[test]
    fn recorded_audit_replay_rejects_missing_or_stale_basis() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"deepseek","responseModel":"deepseek-chat"}}
{"type":"agent_settled"}
"#;
        let (root, wake, decl) = identity_fixture("strict", frames);
        let audit = audit_report_identity(
            &root,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        let report = ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({"identityReconciliation": audit.payload}),
        );
        assert!(recorded_identity_audit_still_current(
            &root,
            &report,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));

        let legacy = ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({"actionId": "report-observed"}),
        );
        assert!(!recorded_identity_audit_still_current(
            &root,
            &legacy,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));

        let mut newer_wake = wake.clone();
        newer_wake.event_id = ulid::Ulid::new().to_string();
        assert!(!recorded_identity_audit_still_current(
            &root,
            &report,
            &[wake, newer_wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
        ));
    }

    #[test]
    fn poll_replay_only_skips_a_current_stored_identity_audit() {
        let frames = r#"{"type":"message_end","message":{"role":"assistant","provider":"deepseek","responseModel":"deepseek-chat"}}
{"type":"agent_settled"}
"#;
        let (root, wake, decl) = identity_fixture("strict", frames);
        let audit = audit_report_identity(
            &root,
            std::slice::from_ref(&wake),
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &decl,
        )
        .unwrap();
        let control = ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({"attemptId": "B235-A0002"}),
        );
        let evidence_path = root.join("reports/B235-REPORT.md");
        let evidence = crate::attempt::EvidenceObservation {
            path: evidence_path.clone(),
            canonical_path: evidence_path.clone(),
            mtime: SystemTime::now(),
            len: 42,
            sha256: "report-sha".to_string(),
            bytes: b"immutable report".to_vec(),
        };
        let report = ledger::event(
            "ReportObserved",
            "runtime:orch",
            Some("B235"),
            Some("r66"),
            serde_json::json!({
                "actionId": "report-observed",
                "attemptId": "B235-A0002",
                "evidencePath": evidence_path.display().to_string(),
                "evidenceSha256": evidence.sha256,
                "evidenceLen": evidence.len,
                "controlEpoch": control.event_id,
                "identityReconciliation": audit.payload,
            }),
        );
        let current = vec![control.clone(), wake.clone(), report.clone()];
        assert!(report_identity_observation_already_recorded(
            &root,
            &current,
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &evidence,
        ));

        let mut legacy = report.clone();
        legacy
            .payload
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("identityReconciliation");
        assert!(!report_identity_observation_already_recorded(
            &root,
            &[control.clone(), wake.clone(), legacy],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &evidence,
        ));

        let mut newer_wake = wake.clone();
        newer_wake.event_id = ulid::Ulid::new().to_string();
        assert!(!report_identity_observation_already_recorded(
            &root,
            &[control, wake, report, newer_wake],
            "r66",
            "B235",
            "B235-A0002",
            "executor-pi",
            &evidence,
        ));
    }
}
