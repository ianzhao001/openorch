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

use crate::{binding, card, collect, gate, ledger, wake};

const GATE_EXECUTED_SCHEMA: (&str, &[&str]) = (
    "GateExecuted",
    &[
        "commandRef",
        "phase",
        "gateRunId",
        "exitCode",
        "durationMs",
        "subjectTreeSha",
        "logSha256",
        "logBytes",
        "toolchainDigest",
        "environmentDigest",
    ],
);

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

    fn b311_fixed_primary_task() -> crate::plan::IrTask {
        crate::plan::IrTask {
            id: "B311".to_string(),
            agent: "executor-desktop".to_string(),
            seed_protocol: "seeded-red".to_string(),
            has_seeds: true,
            write_set: Vec::new(),
            frozen_paths: Vec::new(),
            gates_fast: vec!["testFast".to_string()],
            wall_minutes: 720,
            required_reviews: vec![
                crate::card::RequiredReview {
                    role: "primary".to_string(),
                    agent: "executor-claw".to_string(),
                },
                crate::card::RequiredReview {
                    role: "secondary".to_string(),
                    agent: "executor-pi".to_string(),
                },
            ],
            review_fallbacks: Vec::new(),
            nongate_seats: Vec::new(),
            review_quorum: Some(crate::card::ReviewQuorumPolicy {
                minimum_substantive: 2,
                nongate_may_substitute_failed_formal: true,
                minimum_nongate_pass_for_substitution: 1,
            }),
            primary_pass_alone_satisfies: false,
            required_evidence: vec![SMARTCLAW_FIXED_PRIMARY_EVIDENCE_V1.to_string()],
            bootstrap_pre_signoff_attempt: None,
            depends_on: Vec::new(),
            requirement: crate::plan::IrTaskRequirement::default(),
        }
    }

    fn b311_primary_binding(delivery_id: &str) -> ReviewBinding {
        ReviewBinding {
            path: "coordination/rounds/r82/reviews/B311-A0001-primary-executor-claw.md".to_string(),
            role: "primary".to_string(),
            reviewer: "executor-claw".to_string(),
            verdict: "PASS".to_string(),
            sha256: "a".repeat(64),
            bytes: 100,
            delivery_event_id: Some(delivery_id.to_string()),
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        }
    }

    fn b311_dispatch_fixture(
        actor: &str,
        round: &str,
        task_id: &str,
        attempt_id: &str,
        base_sha: &str,
    ) -> EventRecord {
        crate::ledger::event(
            "DispatchIssued",
            actor,
            Some(task_id),
            Some(round),
            serde_json::json!({
                "attemptId": attempt_id,
                "attemptNo": 1,
                "agent": "executor-desktop",
                "baseSha": base_sha,
            }),
        )
    }

    fn b311_dormant_dispatch_base_from_events(events: &[EventRecord]) -> Result<String> {
        let matches = events
            .iter()
            .filter(|event| {
                event.kind == "DispatchIssued"
                    && event.task_id.as_deref() == Some("B311")
                    && event.round.as_deref() == Some("r82")
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("attemptId"))
                        .and_then(serde_json::Value::as_str)
                        == Some("B311-A0001")
            })
            .collect::<Vec<_>>();
        let [dispatch] = matches.as_slice() else {
            bail!(
                "B311 dormant-base fixture requires exactly one r82/B311/B311-A0001 DispatchIssued"
            );
        };
        if dispatch.actor != "runtime:orch" {
            bail!("B311 dormant-base DispatchIssued actor must be runtime:orch");
        }
        let payload = dispatch
            .payload
            .as_ref()
            .context("B311 dormant-base DispatchIssued missing payload")?;
        if payload.get("attemptNo").and_then(serde_json::Value::as_u64) != Some(1) {
            bail!("B311 dormant-base DispatchIssued attemptNo must be 1");
        }
        let base = payload
            .get("baseSha")
            .and_then(serde_json::Value::as_str)
            .context("B311 dormant-base DispatchIssued missing baseSha")?;
        full_sha(base, "B311 dormant-base DispatchIssued.baseSha")?;
        Ok(base.to_string())
    }

    fn b311_recorded_dormant_dispatch_base(root: &Path) -> Result<String> {
        let ledger = read_ledger(&root.join("coordination/rounds/r82/events.jsonl"))
            .context("read r82 ledger for B311 dormant-base fixture")?;
        if !ledger.bad_lines.is_empty() {
            bail!("r82 ledger contains malformed lines in B311 dormant-base fixture");
        }
        b311_dormant_dispatch_base_from_events(&ledger.events)
    }

    #[test]
    fn b311_dormant_dispatch_base_requires_one_canonical_recorded_tuple() {
        let base = "a".repeat(40);
        let canonical = b311_dispatch_fixture("runtime:orch", "r82", "B311", "B311-A0001", &base);
        assert_eq!(
            b311_dormant_dispatch_base_from_events(std::slice::from_ref(&canonical)).unwrap(),
            base
        );

        assert!(b311_dormant_dispatch_base_from_events(&[]).is_err());
        assert!(b311_dormant_dispatch_base_from_events(&[canonical.clone(), canonical]).is_err());
        for invalid in [
            b311_dispatch_fixture("planner", "r82", "B311", "B311-A0001", &"b".repeat(40)),
            b311_dispatch_fixture("runtime:orch", "r82", "B311", "B311-A0002", &"b".repeat(40)),
            b311_dispatch_fixture("runtime:orch", "r81", "B311", "B311-A0001", &"b".repeat(40)),
            b311_dispatch_fixture("runtime:orch", "r82", "B310", "B311-A0001", &"b".repeat(40)),
            b311_dispatch_fixture("runtime:orch", "r82", "B311", "B311-A0001", &"B".repeat(40)),
        ] {
            assert!(b311_dormant_dispatch_base_from_events(&[invalid]).is_err());
        }
    }

    #[test]
    fn b311_signed_fixed_primary_gate_replays_quorum_without_fallback_substitution() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let base = b311_recorded_dormant_dispatch_base(root).unwrap();
        let head = "b".repeat(40);
        let dispatch = crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B311"),
            Some("r82"),
            serde_json::json!({
                "attemptId": "B311-A0001",
                "attemptNo": 1,
                "agent": "executor-desktop",
                "baseSha": base,
            }),
        );
        let request = crate::ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B311"),
            Some("r82"),
            serde_json::json!({
                "attemptId": "B311-A0001",
                "role": "primary",
                "agent": "executor-claw",
                "reviewedHead": head,
                "wakeId": "11111111-2222-4333-8444-555555555555",
            }),
        );
        let delivery = crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some("B311"),
            Some("r82"),
            serde_json::json!({
                "attemptId": "B311-A0001",
                "role": "primary",
                "agent": "executor-claw",
                "bodyLen": 42,
            }),
        );
        let task = b311_fixed_primary_task();
        let binding = b311_primary_binding(&delivery.event_id);
        let events = vec![dispatch, request, delivery];
        enforce_signed_smartclaw_fixed_primary_pass_v1(
            root,
            &events,
            "r82",
            "B311",
            "B311-A0001",
            &head,
            &task,
            RootVerdict::Pass,
            std::slice::from_ref(&binding),
        )
        .unwrap();

        let mut substituted = binding.clone();
        substituted.substituted_role = Some("primary".to_string());
        assert!(enforce_signed_smartclaw_fixed_primary_pass_v1(
            root,
            &events,
            "r82",
            "B311",
            "B311-A0001",
            &head,
            &task,
            RootVerdict::Pass,
            &[substituted],
        )
        .is_err());

        let fallback = crate::ledger::event(
            "ReviewFallbackSelected",
            "runtime:orch",
            Some("B311"),
            Some("r82"),
            serde_json::json!({
                "attemptId": "B311-A0001",
                "role": "primary",
                "fromAgent": "executor-claw",
                "toAgent": "executor-dsh",
            }),
        );
        let mut before_root = events.clone();
        before_root.insert(2, fallback.clone());
        assert!(enforce_signed_smartclaw_fixed_primary_pass_v1(
            root,
            &before_root,
            "r82",
            "B311",
            "B311-A0001",
            &head,
            &task,
            RootVerdict::Pass,
            std::slice::from_ref(&binding),
        )
        .is_err());

        let root_event = crate::ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("B311"),
            Some("r82"),
            serde_json::json!({"attemptId": "B311-A0001"}),
        );
        let mut archived = events;
        archived.push(root_event);
        archived.push(fallback);
        enforce_signed_smartclaw_fixed_primary_pass_v1(
            root,
            &archived,
            "r82",
            "B311",
            "B311-A0001",
            &head,
            &task,
            RootVerdict::Pass,
            &[binding],
        )
        .expect("post-root unrelated suffix cannot rewrite canonical primary provenance");
    }

    #[test]
    fn b311_signed_fixed_primary_gate_requires_the_exact_panel_terminal_chain() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let head = "b".repeat(40);
        let panel_id = "panel-b311";
        let seat_id = "seat-b311-primary";
        let wake_id = "11111111-2222-4333-8444-555555555555";
        let selected = crate::ledger::runtime_event_v1(
            "r82",
            Some("B311"),
            crate::ledger::RuntimeEventPayloadV1::ReviewPanelSelected(
                crate::ledger::ReviewPanelSelectedPayloadV1 {
                    schema_version: 1,
                    panel_id: panel_id.to_string(),
                    attempt_id: "B311-A0001".to_string(),
                    attempt_no: 1,
                    reviewed_head: head.clone(),
                    policy_base_sha: "c".repeat(40),
                    policy: "review-pool-v1".to_string(),
                    policy_sha256: "d".repeat(64),
                    seat_count: 3,
                    seat_ids: [
                        seat_id.to_string(),
                        "seat-secondary".to_string(),
                        "seat-nongate".to_string(),
                    ],
                },
            ),
        )
        .unwrap();
        let route_payload = crate::ledger::ReviewSeatRoutedPayloadV1 {
            schema_version: 1,
            panel_id: panel_id.to_string(),
            seat_id: seat_id.to_string(),
            generation: 1,
            wake_id: wake_id.to_string(),
            attempt_id: "B311-A0001".to_string(),
            attempt_no: 1,
            role: "primary".to_string(),
            agent: "executor-claw".to_string(),
            lineage: "primary".to_string(),
            reviewed_head: head.clone(),
            policy_base_sha: "c".repeat(40),
            deadline_secs: 3600,
            retry_eligible: true,
            route_kind: "initial".to_string(),
            selected_event_id: selected.event_id.clone(),
            source_seat_id: None,
            source_generation: None,
            source_terminal_event_id: None,
        };
        let route = crate::ledger::runtime_event_v1(
            "r82",
            Some("B311"),
            crate::ledger::RuntimeEventPayloadV1::ReviewSeatRouted(route_payload.clone()),
        )
        .unwrap();
        let terminal = crate::ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some("B311"),
            Some("r82"),
            serde_json::json!({
                "wakeId": wake_id,
                "agent": "executor-claw",
                "completionReason": "natural-exit",
                "terminalSeen": true,
                "turnEnded": true,
                "exitedNaturally": true,
                "hardDeadlineReached": false,
                "signals": [],
                "managedScopeTerminated": true,
                "processTreeTerminated": false,
                "state": "answered",
                "exactReason": "payloads-terminal",
                "finalTextSha256": "e".repeat(64),
                "outputPath": "/repo/.worktrees/review/spool.md",
                "outputSha256": "f".repeat(64),
                "usage": null,
                "usageAbsentReason": "SmartClaw payload terminal exposes no mechanical usage object",
            }),
        );
        let delivery = crate::ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some("B311"),
            Some("r82"),
            serde_json::json!({
                "attemptId": "B311-A0001",
                "role": "primary",
                "agent": "executor-claw",
                "bodyLen": 42,
            }),
        );
        let artifact_name = format!("B311-A0001-{seat_id}-g1-{wake_id}.md");
        let promotion = crate::ledger::runtime_event_v1(
            "r82",
            Some("B311"),
            crate::ledger::RuntimeEventPayloadV1::ReviewSpoolPromoted(
                crate::ledger::ReviewSpoolPromotedPayloadV1 {
                    schema_version: 1,
                    panel_id: panel_id.to_string(),
                    seat_id: seat_id.to_string(),
                    generation: 1,
                    wake_id: wake_id.to_string(),
                    attempt_id: "B311-A0001".to_string(),
                    attempt_no: 1,
                    role: "primary".to_string(),
                    agent: "executor-claw".to_string(),
                    reviewed_head: head.clone(),
                    policy_base_sha: "c".repeat(40),
                    staging_path: format!(
                        ".worktrees/review/.cowork-temp/review-spool/{artifact_name}"
                    ),
                    canonical_path: format!("coordination/rounds/r82/reviews/{artifact_name}"),
                    sha256: "f".repeat(64),
                    bytes: 100,
                    body_len: 42,
                    verdict: "PASS".to_string(),
                    terminal_event_id: terminal.event_id.clone(),
                    delivery_event_id: delivery.event_id.clone(),
                },
            ),
        )
        .unwrap();
        let seat = crate::ledger::runtime_event_v1(
            "r82",
            Some("B311"),
            crate::ledger::RuntimeEventPayloadV1::ReviewSeatTerminated(
                crate::ledger::ReviewSeatTerminatedPayloadV1 {
                    schema_version: 1,
                    panel_id: panel_id.to_string(),
                    seat_id: seat_id.to_string(),
                    generation: 1,
                    wake_id: wake_id.to_string(),
                    attempt_id: "B311-A0001".to_string(),
                    attempt_no: 1,
                    role: "primary".to_string(),
                    agent: "executor-claw".to_string(),
                    lineage: "primary".to_string(),
                    reviewed_head: head.clone(),
                    policy_base_sha: "c".repeat(40),
                    state: "pass".to_string(),
                    terminal_event_id: terminal.event_id.clone(),
                    delivery_event_id: Some(delivery.event_id.clone()),
                    reason: "payloads terminal plus exact artifact".to_string(),
                },
            ),
        )
        .unwrap();
        let task = b311_fixed_primary_task();
        let binding = b311_primary_binding(&delivery.event_id);
        let events = vec![selected, route, terminal.clone(), promotion, delivery, seat];
        enforce_signed_smartclaw_fixed_primary_pass_v1(
            root,
            &events,
            "r82",
            "B311",
            "B311-A0001",
            &head,
            &task,
            RootVerdict::Pass,
            std::slice::from_ref(&binding),
        )
        .unwrap();

        let mut process_tree_forged = events;
        process_tree_forged[2].payload.as_mut().unwrap()["processTreeTerminated"] =
            serde_json::json!(true);
        assert!(enforce_signed_smartclaw_fixed_primary_pass_v1(
            root,
            &process_tree_forged,
            "r82",
            "B311",
            "B311-A0001",
            &head,
            &task,
            RootVerdict::Pass,
            &[binding],
        )
        .is_err());
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
    if active.candidate.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION
        || (active.candidate.verification.mode == "root-manual-fixed-head"
            && active.candidate.verification.adapter == "root-manual")
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NongateInvocationReceipt {
    provider: String,
    model: String,
    effort: String,
    preset: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NongateAttemptReceipt {
    schema_version: u32,
    round: String,
    attempt_id: String,
    agent: String,
    fixed_head: String,
    wake_id: String,
    wake_issued_event_id: String,
    workspace_leased_event_id: String,
    invocation: NongateInvocationReceipt,
    state: String,
    #[serde(default)]
    terminal_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Receipt facts revalidated against exact runtime lease and wake events.
pub(crate) struct ValidatedNongateReceipt {
    /// Closed receipt state (`answered|failed|timedOut|empty`).
    pub state: String,
    /// Observed invocation preset, compared with the signed nongate seat.
    pub preset: String,
    /// Wake identity shared by the receipt and its durable events.
    pub wake_id: String,
    /// Exact `WakeIssued` event referenced by the receipt.
    pub wake_issued_event_id: String,
    /// Exact `WorkspaceLeased` event referenced by the receipt.
    pub workspace_leased_event_id: String,
}

fn nongate_receipt_path(root: &Path, round: &str, attempt_id: &str, agent: &str) -> PathBuf {
    root.join(format!(
        "coordination/runtime/nongate-inbox/{round}/{attempt_id}-{agent}.json"
    ))
}

fn nongate_event_payload_str<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()?
        .get(key)
        .and_then(serde_json::Value::as_str)
}

fn receipt_event<'a>(
    events: &'a [EventRecord],
    event_id: &str,
    kind: &str,
) -> std::result::Result<(usize, &'a EventRecord), String> {
    let mut matches = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == event_id && event.kind == kind);
    let found = matches
        .next()
        .ok_or_else(|| format!("referenced {kind} event is absent"))?;
    if matches.next().is_some() {
        return Err(format!("referenced {kind} event is not unique"));
    }
    Ok(found)
}

/// Revalidate one nongate receipt and return only its exact bound facts.
pub(crate) fn validate_nongate_attempt_receipt_binding(
    root: &Path,
    attempt_id: &str,
    round: &str,
    task_id: &str,
    fixed_head: &str,
    agent: &str,
    events: &[EventRecord],
) -> std::result::Result<ValidatedNongateReceipt, String> {
    let path = nongate_receipt_path(root, round, attempt_id, agent);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("receipt is absent or unreadable: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("receipt is not a regular file".to_string());
    }
    let bytes = fs::read(&path).map_err(|error| format!("receipt read failed: {error}"))?;
    let receipt: NongateAttemptReceipt =
        serde_json::from_slice(&bytes).map_err(|error| format!("receipt JSON invalid: {error}"))?;
    if receipt.schema_version != 1
        || receipt.round != round
        || receipt.attempt_id != attempt_id
        || receipt.agent != agent
        || receipt.fixed_head != fixed_head
    {
        return Err(
            "receipt identity does not match this round/attempt/agent/fixed HEAD".to_string(),
        );
    }
    if receipt.invocation.provider.trim().is_empty()
        || receipt.invocation.model.trim().is_empty()
        || receipt.invocation.effort.trim().is_empty()
        || receipt.invocation.preset.trim().is_empty()
        || receipt.invocation.cwd.trim().is_empty()
        || !Path::new(&receipt.invocation.cwd).is_absolute()
    {
        return Err(
            "receipt invocation must include provider/model/effort/preset and absolute cwd"
                .to_string(),
        );
    }
    match receipt.state.as_str() {
        "answered" => {}
        "failed" | "timedOut" | "empty"
            if receipt
                .terminal_reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty()) => {}
        "failed" | "timedOut" | "empty" => {
            return Err("non-answered receipt is missing its exact terminal reason".to_string());
        }
        _ => return Err("receipt state is not answered/failed/timedOut/empty".to_string()),
    }

    let (lease_position, lease) = receipt_event(
        events,
        &receipt.workspace_leased_event_id,
        "WorkspaceLeased",
    )?;
    let (wake_position, wake) = receipt_event(events, &receipt.wake_issued_event_id, "WakeIssued")?;
    if lease_position >= wake_position {
        return Err("WorkspaceLeased must precede the bound WakeIssued".to_string());
    }
    for (kind, event) in [("WorkspaceLeased", lease), ("WakeIssued", wake)] {
        if event.actor != "runtime:orch"
            || event.round.as_deref() != Some(round)
            || event.task_id.as_deref() != Some(task_id)
            || nongate_event_payload_str(event, "agent") != Some(agent)
            || nongate_event_payload_str(event, "attemptId") != Some(attempt_id)
            || nongate_event_payload_str(event, "wakeId") != Some(receipt.wake_id.as_str())
        {
            return Err(format!(
                "bound {kind} does not match runtime actor/task/round/attempt/agent/wakeId"
            ));
        }
    }
    if nongate_event_payload_str(lease, "role") != Some("nongate")
        || nongate_event_payload_str(lease, "reviewedHead") != Some(fixed_head)
    {
        return Err("bound WorkspaceLeased is not the exact nongate fixed-HEAD lease".to_string());
    }
    let leased_worktree = lease
        .payload
        .as_ref()
        .and_then(|payload| payload.get("paths"))
        .and_then(|paths| paths.get("worktree"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "bound WorkspaceLeased has no worktree path".to_string())?;
    let expected_cwd = root.join(leased_worktree);
    if Path::new(&receipt.invocation.cwd) != expected_cwd {
        return Err("receipt cwd does not match the bound WorkspaceLeased worktree".to_string());
    }
    Ok(ValidatedNongateReceipt {
        state: receipt.state,
        preset: receipt.invocation.preset,
        wake_id: receipt.wake_id,
        wake_issued_event_id: receipt.wake_issued_event_id,
        workspace_leased_event_id: receipt.workspace_leased_event_id,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootVerdict {
    Pass,
    Fail,
    Blocked,
}

/// Version anchor for the collect-to-root reuse identity and decision contract.
pub const ROOT_GATE_REUSE_CONTRACT_V1: u32 = 1;

/// One independently validated collect gate that may satisfy the root phase.
///
/// The source event and raw-log binding are retained so reuse never degrades into an
/// untraceable statement that a command was green at some unspecified time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReusedGateV1 {
    /// Semantic command reference in the collect receipt's exact invocation order.
    pub name: String,
    /// Observed child exit code; only zero can be reused.
    pub exit_code: i32,
    /// Durable collect-phase `GateExecuted` event identity.
    pub source_event_id: String,
    /// SHA-256 of the independently reread raw log CAS object.
    pub log_sha256: String,
    /// Exact byte length paired with the raw-log digest.
    pub log_bytes: u64,
}

/// CAS-validated collect proof and every signed identity dimension root must compare.
///
/// Attempt and candidate-commit lineage are checked by the production loader before this
/// projection is created; the tree, contract, command, toolchain, environment, and source gates
/// remain explicit here so the pure decision can be exhaustively degraded in tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectGateBundleV1 {
    /// Git tree proven by collect; moving main is intentionally absent from this identity.
    pub subject_tree_sha: String,
    /// Active signed ROUND-IR revision at collect time.
    pub ir_revision: u32,
    /// Canonical digest of that ROUND-IR revision.
    pub validation_digest: String,
    /// SHA-256 of the signed project binding.
    pub binding_sha256: String,
    /// SHA-256 of this task's signed card.
    pub task_card_sha256: String,
    /// Digest of the exact ordered argv sequence reconstructed from the attempt policy base.
    pub resolved_command_digest: String,
    /// Digest of the Cargo/rustc toolchain that ran collect.
    pub toolchain_digest: String,
    /// Digest of the machine, sandbox, overlay, and build-affecting environment.
    pub environment_digest: String,
    /// Ordered green source observations; repeated command refs remain distinct by event id.
    pub gates: Vec<ReusedGateV1>,
}

/// Root's independently captured tree, signed contract, command, toolchain, and environment.
///
/// Every field is captured or recomputed at root rather than copied from the receipt. Current
/// main is deliberately excluded because normal review artifacts advance main without changing
/// the candidate proof identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RootGateSubjectV1 {
    /// Git tree root is about to authorize.
    pub subject_tree_sha: String,
    /// Current active signed ROUND-IR revision.
    pub ir_revision: u32,
    /// Canonical digest of the current active ROUND-IR.
    pub validation_digest: String,
    /// SHA-256 of the current signed project binding.
    pub binding_sha256: String,
    /// SHA-256 of the current signed task card.
    pub task_card_sha256: String,
    /// Independently recomputed ordered argv digest.
    pub resolved_command_digest: String,
    /// Independently captured Cargo/rustc toolchain digest.
    pub toolchain_digest: String,
    /// Independently captured execution-environment digest.
    pub environment_digest: String,
}

/// Closed root-gate decision: reuse every exact green source or run the real merge lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootGateReuseDecisionV1 {
    /// All identity dimensions and every readable green source matched exactly.
    Reuse(Vec<ReusedGateV1>),
    /// Reuse is unsafe; `reason` is one of the typed durable miss classes.
    Execute {
        /// Closed miss class consumed by `GateReuseMiss` (`input-tree`, `contract`, `command`,
        /// `toolchain`, or `environment`).
        reason: String,
    },
}

/// Compare a validated collect bundle with root's independently captured subject.
///
/// The function performs no IO: callers must first use the production collect loader, which
/// rereads the receipt attestation and every raw-log CAS object. Invalid or red source tuples are
/// classified as a contract miss and can never reach `Reuse`.
pub fn plan_root_gate_reuse_v1(
    bundle: &CollectGateBundleV1,
    subject: &RootGateSubjectV1,
) -> RootGateReuseDecisionV1 {
    let execute = |reason: &str| RootGateReuseDecisionV1::Execute {
        reason: reason.to_string(),
    };
    if bundle.subject_tree_sha != subject.subject_tree_sha {
        return execute("input-tree");
    }
    if bundle.ir_revision != subject.ir_revision
        || bundle.validation_digest != subject.validation_digest
        || bundle.binding_sha256 != subject.binding_sha256
        || bundle.task_card_sha256 != subject.task_card_sha256
    {
        return execute("contract");
    }
    if bundle.resolved_command_digest != subject.resolved_command_digest {
        return execute("command");
    }
    if bundle.toolchain_digest != subject.toolchain_digest {
        return execute("toolchain");
    }
    if bundle.environment_digest != subject.environment_digest {
        return execute("environment");
    }
    let valid_hex = |value: &str, len: usize| {
        value.len() == len
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    let mut source_ids = BTreeSet::new();
    if !valid_hex(&bundle.subject_tree_sha, 40)
        || bundle.ir_revision == 0
        || !valid_hex(&bundle.validation_digest, 64)
        || !valid_hex(&bundle.binding_sha256, 64)
        || !valid_hex(&bundle.task_card_sha256, 64)
        || !valid_hex(&bundle.resolved_command_digest, 64)
        || !valid_hex(&bundle.toolchain_digest, 64)
        || !valid_hex(&bundle.environment_digest, 64)
        || bundle.gates.is_empty()
        || bundle.gates.iter().any(|gate| {
            gate.name.trim().is_empty()
                || gate.exit_code != 0
                || gate.source_event_id.trim().is_empty()
                || !source_ids.insert(gate.source_event_id.as_str())
                || !valid_hex(&gate.log_sha256, 64)
                || gate.log_bytes == 0
        })
    {
        return execute("contract");
    }
    RootGateReuseDecisionV1::Reuse(bundle.gates.clone())
}

/// Classification of one review observation before root quorum evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewResultState {
    /// A substantive PASS from a formal primary or secondary reviewer.
    FormalPass,
    /// A substantive FAIL from a formal reviewer; it is a monotonic veto.
    FormalFail,
    /// A substantive PASS from a signed nongate reviewer.
    NongatePass,
    /// A substantive FAIL from a nongate reviewer; it is a monotonic veto.
    NongateFail,
    /// A substantive BLOCKED finding from either review class.
    Blocked,
    /// A formal seat whose managed channel has not reached a terminal fact.
    Pending,
    /// An authenticated terminal channel failure that contributes no voice.
    ChannelError,
    /// An exact managed operational terminal that closes its formal seat but
    /// can never authorize fallback or nongate substitution.
    OperationalError,
    /// A terminal invocation that produced no substantive artifact.
    Empty,
    /// A terminal invocation that reached its signed deadline.
    TimedOut,
}

/// One exact review voice or channel observation bound to a delivery identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewResult {
    /// Reviewer identity; one agent can contribute at most one voice.
    pub agent: String,
    /// Review role (`primary`, `secondary`, or `nongate`).
    pub role: String,
    /// Canonical attempt whose immutable candidate was reviewed.
    pub attempt_id: String,
    /// Full immutable candidate commit.
    pub reviewed_head: String,
    /// Durable delivery or channel event identity; one event is one voice.
    pub delivery_event_id: String,
    /// Semantic state observed for this exact tuple.
    pub state: ReviewResultState,
}

/// Result of closed review-quorum evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewQuorumDecision {
    /// The signed minimum is met and every formal seat is closed.
    Satisfied,
    /// A substantive FAIL or BLOCKED finding vetoes PASS.
    BlockedByFinding,
    /// More terminal or substantive facts are required.
    Insufficient,
    /// The supplied identities double-count or cross attempt/head boundaries.
    Invalid,
}

/// Signed-policy mode selected for a task during replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewContractMode {
    /// Historical cards: every declared formal seat is required exactly.
    LegacyStrict,
    /// New cards: evaluate unique voices under their signed quorum policy.
    Quorum,
    /// Attempt-base policy selects durable panel routes and generation-scoped artifacts.
    Panel,
}

/// Resolve the review contract from the attempt's first DispatchIssued base.
/// Historical attempt bases that predate a committed policy binding or
/// ROUND-IR retain their signed legacy/quorum behavior; a present but malformed
/// policy artifact still fails closed.
pub fn review_contract_mode_for_attempt(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
    task: &crate::plan::IrTask,
) -> Result<ReviewContractMode> {
    let resolved = match crate::plan::resolve_attempt_runtime_policy(
        root,
        round,
        events,
        task_id,
        attempt_id,
        "review-pool-v1",
    ) {
        Ok(resolved) => resolved,
        Err(error)
            if error
                    .to_string()
                .contains("缺 runtime policy review-pool-v1")
                || error.to_string().contains("缺 runtimePolicies envelope")
                || {
                    let detail = format!("{error:#}");
                    (detail.contains("读取 policy-base committed PROJECT-BINDING 失败")
                        || detail.contains("读取 policy-base committed ROUND-IR 失败"))
                        && (detail.contains("does not exist in")
                            || detail.contains("exists on disk, but not in"))
                } =>
        {
            return review_contract_mode_for_resolved_state(
                task,
                crate::plan::RuntimePolicyStateV1::Dormant,
            )
        }
        Err(error) => return Err(error).context("resolve attempt review policy failed"),
    };
    review_contract_mode_for_resolved_state(task, resolved.state)
}

fn review_contract_mode_for_resolved_state(
    task: &crate::plan::IrTask,
    state: crate::plan::RuntimePolicyStateV1,
) -> Result<ReviewContractMode> {
    if task.primary_pass_alone_satisfies && task.review_quorum.is_none() {
        bail!("signed primaryPassAloneSatisfies 缺 reviewQuorum");
    }
    let baseline = if task.review_quorum.is_some() {
        ReviewContractMode::Quorum
    } else {
        ReviewContractMode::LegacyStrict
    };
    match state {
        crate::plan::RuntimePolicyStateV1::Dormant => Ok(baseline),
        crate::plan::RuntimePolicyStateV1::Active => {
            if task.primary_pass_alone_satisfies {
                bail!("active review-pool-v1 不支持 signed primaryPassAloneSatisfies");
            }
            let quorum = task
                .review_quorum
                .as_ref()
                .context("active review-pool-v1 task 缺 signed reviewQuorum compatibility floor")?;
            if quorum.minimum_substantive < 2
                || !quorum.nongate_may_substitute_failed_formal
                || quorum.minimum_nongate_pass_for_substitution == 0
            {
                bail!("active review-pool-v1 与 signed task quorum floor 不兼容");
            }
            Ok(ReviewContractMode::Panel)
        }
    }
}

fn evaluate_seat_closure(
    policy: &crate::card::ReviewQuorumPolicy,
    pending_formal: bool,
    formal_channel_errors: usize,
    nongate_passes: usize,
) -> bool {
    if pending_formal {
        return false;
    }
    formal_channel_errors == 0
        || (policy.nongate_may_substitute_failed_formal
            && nongate_passes >= policy.minimum_nongate_pass_for_substitution
            && formal_channel_errors <= nongate_passes)
}

/// Evaluate unique substantive voices without trusting list order.
///
/// The first observation establishes the attempt/head tuple; any mismatch,
/// duplicate agent, duplicate delivery event, or role/state mismatch is
/// invalid. Findings are scanned before PASS counts so a trailing FAIL can
/// never be overwritten by earlier positive voices.
pub fn evaluate_review_quorum(
    policy: &crate::card::ReviewQuorumPolicy,
    results: &[ReviewResult],
) -> ReviewQuorumDecision {
    evaluate_review_quorum_internal(policy, None, results).0
}

/// Evaluate the signed task's quorum, including its narrow default-primary
/// exception when enabled.
///
/// Unlike [`evaluate_review_quorum`], this production entry point verifies
/// that every formal observation belongs to exactly one signed seat and that
/// every signed formal seat is represented. The exception accepts only the
/// task's default primary reviewer, never a fallback or nongate voice, and a
/// pending formal seat still makes the result insufficient.
pub fn evaluate_review_quorum_for_task(
    task: &crate::plan::IrTask,
    results: &[ReviewResult],
) -> ReviewQuorumDecision {
    let Some(policy) = task.review_quorum.as_ref() else {
        return ReviewQuorumDecision::Invalid;
    };
    evaluate_review_quorum_internal(policy, Some(task), results).0
}

fn evaluate_review_quorum_internal(
    policy: &crate::card::ReviewQuorumPolicy,
    task: Option<&crate::plan::IrTask>,
    results: &[ReviewResult],
) -> (ReviewQuorumDecision, bool) {
    if policy.minimum_substantive < 2 || policy.minimum_nongate_pass_for_substitution == 0 {
        return (ReviewQuorumDecision::Invalid, false);
    }
    let Some(first) = results.first() else {
        return (ReviewQuorumDecision::Insufficient, false);
    };
    let mut agents = BTreeSet::new();
    let mut deliveries = BTreeSet::new();
    let mut formal_roles = BTreeSet::new();
    let mut substantive = 0usize;
    let mut nongate_passes = 0usize;
    let mut formal_channel_errors = 0usize;
    let mut formal_operational_errors = 0usize;
    let mut pending_formal = false;
    let mut finding = false;
    for result in results {
        if result.agent.trim().is_empty()
            || result.delivery_event_id.trim().is_empty()
            || result.attempt_id != first.attempt_id
            || result.reviewed_head != first.reviewed_head
            || !agents.insert(result.agent.as_str())
            || !deliveries.insert(result.delivery_event_id.as_str())
        {
            return (ReviewQuorumDecision::Invalid, false);
        }
        let formal = matches!(result.role.as_str(), "primary" | "secondary");
        let nongate = result.role == "nongate";
        if !formal && !nongate {
            return (ReviewQuorumDecision::Invalid, false);
        }
        if let Some(task) = task {
            if formal {
                let signed = task
                    .required_reviews
                    .iter()
                    .filter(|required| required.role == result.role)
                    .collect::<Vec<_>>();
                let [required] = signed.as_slice() else {
                    return (ReviewQuorumDecision::Invalid, false);
                };
                if (required.agent != result.agent
                    && signed_review_fallback(task, &result.role) != Some(result.agent.as_str()))
                    || !formal_roles.insert(result.role.as_str())
                {
                    return (ReviewQuorumDecision::Invalid, false);
                }
            } else if !task
                .nongate_seats
                .iter()
                .any(|seat| seat.agent == result.agent)
            {
                return (ReviewQuorumDecision::Invalid, false);
            }
        }
        match result.state {
            ReviewResultState::FormalPass if formal => substantive += 1,
            ReviewResultState::FormalFail if formal => finding = true,
            ReviewResultState::NongatePass if nongate => {
                substantive += 1;
                nongate_passes += 1;
            }
            ReviewResultState::NongateFail if nongate => finding = true,
            ReviewResultState::Blocked => finding = true,
            ReviewResultState::Pending if formal => pending_formal = true,
            ReviewResultState::ChannelError
            | ReviewResultState::Empty
            | ReviewResultState::TimedOut
                if formal =>
            {
                formal_channel_errors += 1;
            }
            ReviewResultState::OperationalError if formal => {
                formal_operational_errors += 1;
            }
            ReviewResultState::ChannelError
            | ReviewResultState::OperationalError
            | ReviewResultState::Empty
            | ReviewResultState::TimedOut
                if nongate => {}
            _ => return (ReviewQuorumDecision::Invalid, false),
        }
    }
    if finding {
        return (ReviewQuorumDecision::BlockedByFinding, false);
    }
    if let Some(task) = task {
        if task
            .required_reviews
            .iter()
            .any(|required| !formal_roles.contains(required.role.as_str()))
        {
            return (ReviewQuorumDecision::Insufficient, false);
        }
    }
    let primary_pass_alone = task.is_some_and(|task| {
        if !task.primary_pass_alone_satisfies || pending_formal {
            return false;
        }
        let primary = task
            .required_reviews
            .iter()
            .filter(|required| required.role == "primary")
            .collect::<Vec<_>>();
        let [primary] = primary.as_slice() else {
            return false;
        };
        results.iter().any(|result| {
            result.role == "primary"
                && result.agent == primary.agent
                && result.state == ReviewResultState::FormalPass
        })
    });
    if formal_operational_errors > 0 && !primary_pass_alone {
        return (ReviewQuorumDecision::Insufficient, false);
    }
    if !primary_pass_alone
        && !evaluate_seat_closure(
            policy,
            pending_formal,
            formal_channel_errors,
            nongate_passes,
        )
    {
        return (ReviewQuorumDecision::Insufficient, false);
    }
    if primary_pass_alone || substantive >= policy.minimum_substantive {
        (ReviewQuorumDecision::Satisfied, primary_pass_alone)
    } else {
        (ReviewQuorumDecision::Insufficient, false)
    }
}

#[cfg(test)]
mod review_quorum_contract_tests {
    use super::*;

    fn substitution() -> ReviewSeatSubstitutionBinding {
        ReviewSeatSubstitutionBinding {
            role: "primary".to_string(),
            from_agent: "executor-opencode".to_string(),
            to_agent: "executor-dsh".to_string(),
            reviewed_head: "a".repeat(40),
            source_terminal_event_id: "terminal-1".to_string(),
            nongate_delivery_event_id: "nongate-1".to_string(),
        }
    }

    fn substitution_event(verdict_event_id: &str) -> EventRecord {
        let expected = substitution();
        ledger::event(
            "ReviewSeatSubstituted",
            "runtime:orch",
            Some("B303"),
            Some("r79"),
            serde_json::json!({
                "attemptId": "B303-A0001",
                "role": expected.role,
                "fromAgent": expected.from_agent,
                "toAgent": expected.to_agent,
                "reviewedHead": expected.reviewed_head,
                "sourceTerminalEventId": expected.source_terminal_event_id,
                "nongateDeliveryEventId": expected.nongate_delivery_event_id,
                "verdictEventId": verdict_event_id,
            }),
        )
    }

    fn substitution_facts() -> Vec<EventRecord> {
        let mut terminal = ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some("B303"),
            Some("r79"),
            serde_json::json!({
                "agent": "executor-opencode",
                "managedScopeTerminated": true,
            }),
        );
        terminal.event_id = "terminal-1".to_string();
        let mut nongate = ledger::event(
            "NongateReviewDelivered",
            "runtime:orch",
            Some("B303"),
            Some("r79"),
            serde_json::json!({
                "attemptId": "B303-A0001",
                "role": "nongate",
                "agent": "executor-dsh",
                "reviewedHead": "a".repeat(40),
            }),
        );
        nongate.event_id = "nongate-1".to_string();
        vec![terminal, nongate]
    }

    #[test]
    fn substitution_is_adjacent_bidirectional_and_actor_bound() {
        let verdict = ledger::event(
            "VerdictIssued",
            "verifier:root",
            Some("B303"),
            Some("r79"),
            serde_json::json!({}),
        );
        let event = substitution_event(&verdict.event_id);
        let mut events = substitution_facts();
        events.extend([event.clone(), verdict.clone()]);
        validate_review_substitution_events(
            &events,
            "r79",
            "B303",
            "B303-A0001",
            3,
            &[substitution()],
            Some(&verdict.event_id),
        )
        .unwrap();

        let mut wrong_actor = event.clone();
        wrong_actor.actor = "planner".to_string();
        let mut wrong_actor_events = substitution_facts();
        wrong_actor_events.extend([wrong_actor, verdict.clone()]);
        assert!(validate_review_substitution_events(
            &wrong_actor_events,
            "r79",
            "B303",
            "B303-A0001",
            3,
            &[substitution()],
            Some(&verdict.event_id),
        )
        .is_err());
        let mut wrong_order = substitution_facts();
        wrong_order.extend([verdict, event]);
        assert!(validate_review_substitution_events(
            &wrong_order,
            "r79",
            "B303",
            "B303-A0001",
            2,
            &[substitution()],
            None,
        )
        .is_err());
    }

    #[test]
    fn historical_review_binding_serialization_omits_all_new_keys() {
        let binding = ReviewBinding {
            path: "review.md".to_string(),
            role: "primary".to_string(),
            reviewer: "executor-opencode".to_string(),
            verdict: "PASS".to_string(),
            sha256: "b".repeat(64),
            bytes: 9,
            delivery_event_id: None,
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        };
        let value = serde_json::to_value(&binding).unwrap();
        for key in [
            "deliveryEventId",
            "substitutedRole",
            "substitutedAgent",
            "sourceTerminalEventId",
        ] {
            assert!(value.get(key).is_none(), "legacy binding leaked {key}");
        }
        assert_eq!(
            serde_json::from_value::<ReviewBinding>(value).unwrap(),
            binding
        );
    }

    #[test]
    fn expected_main_accepts_only_canonical_review_quorum_events() {
        let good = substitution_event("verdict-1");
        assert!(canonical_review_quorum_suffix_event(&good, "r79").unwrap());
        let mut forged = good;
        forged.actor = "planner".to_string();
        assert!(canonical_review_quorum_suffix_event(&forged, "r79").is_err());
    }

    fn primary_alone_task() -> crate::plan::IrTask {
        crate::plan::IrTask {
            id: "B9313".to_string(),
            agent: "executor-desktop".to_string(),
            seed_protocol: "verify-only".to_string(),
            has_seeds: false,
            write_set: Vec::new(),
            frozen_paths: Vec::new(),
            gates_fast: vec!["testFast".to_string()],
            wall_minutes: 30,
            required_reviews: vec![
                crate::card::RequiredReview {
                    role: "primary".to_string(),
                    agent: "executor-claw".to_string(),
                },
                crate::card::RequiredReview {
                    role: "secondary".to_string(),
                    agent: "executor-pi".to_string(),
                },
            ],
            review_fallbacks: Vec::new(),
            nongate_seats: vec![crate::card::NongateSeat {
                agent: "executor-antigravity".to_string(),
                preset: "headless".to_string(),
            }],
            review_quorum: Some(crate::card::ReviewQuorumPolicy {
                minimum_substantive: 2,
                nongate_may_substitute_failed_formal: true,
                minimum_nongate_pass_for_substitution: 1,
            }),
            primary_pass_alone_satisfies: true,
            required_evidence: Vec::new(),
            bootstrap_pre_signoff_attempt: None,
            depends_on: Vec::new(),
            requirement: crate::plan::IrTaskRequirement::default(),
        }
    }

    fn archived_primary_alone_fixture() -> (Vec<EventRecord>, RootVerdictPayload) {
        let attempt = "B9313-A0001";
        let head = "a".repeat(40);
        let primary_wake_id = "wake-primary";
        let secondary_wake_id = "wake-secondary";
        let primary_wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B9313"),
            Some("r9996"),
            serde_json::json!({
                "attemptId": attempt,
                "agent": "executor-claw",
                "wakeId": primary_wake_id,
            }),
        );
        let primary_request = ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B9313"),
            Some("r9996"),
            serde_json::json!({
                "attemptId": attempt,
                "role": "primary",
                "agent": "executor-claw",
                "wakeId": primary_wake_id,
                "reviewedHead": head,
            }),
        );
        let primary_delivery = ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some("B9313"),
            Some("r9996"),
            serde_json::json!({
                "attemptId": attempt,
                "role": "primary",
                "agent": "executor-claw",
                "bodyLen": 24,
            }),
        );
        let secondary_wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B9313"),
            Some("r9996"),
            serde_json::json!({
                "attemptId": attempt,
                "agent": "executor-pi",
                "wakeId": secondary_wake_id,
            }),
        );
        let secondary_request = ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B9313"),
            Some("r9996"),
            serde_json::json!({
                "attemptId": attempt,
                "role": "secondary",
                "agent": "executor-pi",
                "wakeId": secondary_wake_id,
                "reviewedHead": head,
            }),
        );
        let secondary_terminal = ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some("B9313"),
            Some("r9996"),
            serde_json::json!({
                "agent": "executor-pi",
                "wakeId": secondary_wake_id,
                "outcomeClass": "TruncatedNoTerminal",
                "managedScopeTerminated": true,
            }),
        );
        let payload = RootVerdictPayload {
            verdict: "PASS".to_string(),
            reason: None,
            ir_revision: 1,
            validation_digest: "b".repeat(64),
            attempt_id: attempt.to_string(),
            attempt_no: 1,
            implementer_agent: "executor-desktop".to_string(),
            head_sha: head,
            main_head_sha: "c".repeat(40),
            collect_completed_event_id: "collect".to_string(),
            bootstrap_pre_signoff_attempt: None,
            reviews: vec![ReviewBinding {
                path: "primary.md".to_string(),
                role: "primary".to_string(),
                reviewer: "executor-claw".to_string(),
                verdict: "PASS".to_string(),
                sha256: "d".repeat(64),
                bytes: 100,
                delivery_event_id: Some(primary_delivery.event_id.clone()),
                substituted_role: None,
                substituted_agent: None,
                source_terminal_event_id: None,
            }],
            evidence: Vec::new(),
            gates: Vec::new(),
        };
        (
            vec![
                primary_wake,
                primary_request,
                primary_delivery,
                secondary_wake,
                secondary_request,
                secondary_terminal,
            ],
            payload,
        )
    }

    #[test]
    fn archived_primary_only_rebuild_requires_exact_secondary_request_and_terminal() {
        let root = crate::util::test_scratch_dir("b313-archived-primary-alone");
        let task = primary_alone_task();
        let (events, payload) = archived_primary_alone_fixture();
        validate_archived_review_transition_chain(
            &events,
            "r9996",
            "B9313",
            events.len(),
            &payload,
            &task,
        )
        .unwrap();

        let request_position = events
            .iter()
            .position(|event| {
                event.kind == "ReviewRequested"
                    && nongate_event_payload_str(event, "role") == Some("secondary")
            })
            .unwrap();
        let terminal_position = events
            .iter()
            .position(|event| {
                event.kind == "ManagedWakeTerminated"
                    && nongate_event_payload_str(event, "agent") == Some("executor-pi")
            })
            .unwrap();
        let mut variants = Vec::new();
        let mut missing_request = events.clone();
        missing_request.remove(request_position);
        variants.push(missing_request);
        let mut missing_terminal = events.clone();
        missing_terminal.remove(terminal_position);
        variants.push(missing_terminal);
        let mut cross_attempt = events.clone();
        cross_attempt[request_position].payload.as_mut().unwrap()["attemptId"] =
            serde_json::json!("B9313-A0002");
        variants.push(cross_attempt);
        let mut head_drift = events.clone();
        head_drift[request_position].payload.as_mut().unwrap()["reviewedHead"] =
            serde_json::json!("e".repeat(40));
        variants.push(head_drift);
        let mut terminal_before_request = events.clone();
        let terminal = terminal_before_request.remove(terminal_position);
        terminal_before_request.insert(request_position, terminal);
        variants.push(terminal_before_request);
        let mut duplicate_wake = events.clone();
        duplicate_wake.insert(
            request_position,
            ledger::event(
                "WakeIssued",
                "runtime:orch",
                Some("B9313"),
                Some("r9996"),
                serde_json::json!({
                    "attemptId": "B9313-A0002",
                    "agent": "executor-pi",
                    "wakeId": "wake-secondary",
                }),
            ),
        );
        variants.push(duplicate_wake);

        for variant in variants {
            let error = validate_archived_review_transition_chain(
                &variant,
                "r9996",
                "B9313",
                variant.len(),
                &payload,
                &task,
            )
            .unwrap_err();
            let detail = error.to_string();
            assert!(
                detail.contains("archived root PASS review quorum 未满足: Insufficient")
                    || (detail.contains("formal ReviewRequested") && detail.contains("漂移"))
                    || detail.contains("WakeIssued")
                    || detail.contains("ManagedWakeTerminated"),
                "{error:#}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn active_panel_rejects_the_primary_pass_alone_switch() {
        let task = primary_alone_task();
        let error = review_contract_mode_for_resolved_state(
            &task,
            crate::plan::RuntimePolicyStateV1::Active,
        )
        .unwrap_err();
        assert!(error.to_string().contains("primaryPassAloneSatisfies"));
        assert_eq!(
            review_contract_mode_for_resolved_state(
                &task,
                crate::plan::RuntimePolicyStateV1::Dormant,
            )
            .unwrap(),
            ReviewContractMode::Quorum
        );
    }
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
    /// Durable delivery event that minted this unique voice. Historical root
    /// payloads omit it and retain their strict byte-compatible replay shape.
    #[serde(
        rename = "deliveryEventId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub delivery_event_id: Option<String>,
    /// Formal role closed by this nongate voice, when the binding also backs a
    /// durable `ReviewSeatSubstituted` fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substituted_role: Option<String>,
    /// Formal reviewer whose terminal channel is closed by this nongate voice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub substituted_agent: Option<String>,
    /// Managed terminal event proving the substituted formal channel ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_terminal_event_id: Option<String>,
}

/// Durable root binding for one nongate-backed formal-seat substitution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSeatSubstitutionBinding {
    /// Formal role whose current managed channel ended without an artifact.
    pub role: String,
    /// Formal reviewer whose failed channel is being closed.
    pub from_agent: String,
    /// Signed nongate reviewer supplying the substantive PASS voice.
    pub to_agent: String,
    /// Immutable candidate commit shared by both observations.
    pub reviewed_head: String,
    /// Authenticated managed terminal event for the failed formal channel.
    pub source_terminal_event_id: String,
    /// Durable nongate delivery event supplying the substitution.
    pub nongate_delivery_event_id: String,
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
/// Immutable binding from a root verdict to one concrete gate log.
///
/// New receipts carry the runtime-minted run identity so two byte-identical reruns remain
/// distinguishable; the optional form preserves validation of verdicts recorded before B297.
pub struct VerdictGateBinding {
    /// Command reference resolved from the signed task gate list.
    pub name: String,
    /// Process exit code observed after the gate child was reaped.
    pub exit_code: i32,
    /// SHA-256 of the complete raw gate log.
    pub log_sha256: String,
    /// Raw gate-log length bound alongside its digest.
    pub log_bytes: u64,
    /// Runtime-minted identity of the phase-scoped gate execution, absent only on legacy verdicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_run_id: Option<String>,
    /// Durable `GateReused` event satisfying this binding without a second child execution.
    ///
    /// New bindings carry exactly one of this field and `gate_run_id`; both absent is accepted
    /// only for historical verdicts written before phase-scoped run identities existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_event_id: Option<String>,
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
    pub plan_signed_off_event_id: String,
    pub attempt_id: String,
    pub attempt_no: usize,
    pub implementer_agent: String,
    pub head_sha: String,
    pub expected_main_sha: String,
    pub merge_sha: String,
    pub ir_revision: u32,
    pub validation_digest: String,
    pub task_card_sha256: String,
    pub reviews: Vec<ReviewBinding>,
    pub already_recorded: bool,
}

/// Decode the exact historical payload of a formerly authorized frozen-contract
/// mutation. No current writer constructs or emits this event.  The signed declaration is repeated
/// verbatim, while the remaining fields bind it to the post-green lifecycle
/// that made it Effective.  `deny_unknown_fields` is intentional: a forged
/// suffix cannot smuggle an unvalidated authorization arm beside known keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenContractSupersededPayload {
    pub target: String,
    pub initiator: String,
    pub original_seed_relocated: card::FrozenSeedRelocationAnchor,
    pub effective_anchor: card::FrozenEffectiveAnchor,
    pub old_file_sha256: String,
    pub new_file_sha256: String,
    /// Historical old literal digest; present only for the literal-swap
    /// evolution and serialized at its original top-level key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_literal_sha256: Option<String>,
    /// Historical new literal digest; present only for the literal-swap
    /// evolution and serialized at its original top-level key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_literal_sha256: Option<String>,
    /// Historical subject-prefix binding; present only for the literal-swap
    /// evolution and serialized at its original top-level key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_prefix: Option<card::FrozenSubjectPrefix>,
    /// Ordered structured evolution, mutually exclusive with all three
    /// literal-swap fields and retained verbatim through durable replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_evolution: Option<card::FrozenStructuredEvolution>,
    /// Historical recovery attempt, omitted by planner-adjudicated payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_attempt: Option<card::FrozenBlockedAttempt>,
    /// Historical recorded replacement, omitted by planner-adjudicated
    /// payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<card::FrozenRecordedReplacement>,
    /// Candidate-bound planner adjudication, absent from every historical
    /// recovery payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<card::FrozenPlannerAdjudicatedAuthorization>,
    pub dispositions: Vec<card::FrozenContractDisposition>,
    pub reviews: Vec<card::RequiredReview>,
    pub review_bindings: Vec<ReviewBinding>,
    pub prior_main_sha: String,
    pub candidate_sha: String,
    pub merge_sha: String,
    pub effective_main_sha: String,
    pub ir_revision: u32,
    pub validation_digest: String,
    pub task_card_sha256: String,
    pub plan_signed_off_event_id: String,
    pub verdict_event_id: String,
    pub task_recorded_event_id: String,
}

impl FrozenContractSupersededPayload {
    /// Reconstruct the exact signed card declaration carried by this payload.
    /// Replay compares the result structurally, including which authorization
    /// arm is present, so dropping or mixing planner evidence fails closed.
    pub fn signed_declaration(&self) -> card::FrozenContractSupersession {
        card::FrozenContractSupersession {
            target: self.target.clone(),
            initiator: self.initiator.clone(),
            original_seed_relocated: self.original_seed_relocated.clone(),
            effective_anchor: self.effective_anchor.clone(),
            old_file_sha256: self.old_file_sha256.clone(),
            new_file_sha256: self.new_file_sha256.clone(),
            old_literal_sha256: self.old_literal_sha256.clone(),
            new_literal_sha256: self.new_literal_sha256.clone(),
            subject_prefix: self.subject_prefix.clone(),
            structured_evolution: self.structured_evolution.clone(),
            blocked_attempt: self.blocked_attempt.clone(),
            replacement: self.replacement.clone(),
            authorization: self.authorization.clone(),
            dispositions: self.dispositions.clone(),
            reviews: self.reviews.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordGateRelaxedPayload {
    stage: String,
    merge_sha: String,
    tip_sha: String,
    reason: String,
    files: Vec<String>,
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
    #[serde(rename = "seatId", default)]
    seat_id: Option<String>,
    #[serde(default)]
    generation: Option<u32>,
    #[serde(rename = "wakeId", default)]
    wake_id: Option<String>,
    #[serde(rename = "policyBaseSha", default)]
    policy_base_sha: Option<String>,
    #[serde(flatten)]
    ignored: BTreeMap<String, serde_yaml::Value>,
}

/// Four generation-scoped fields that extend the immutable base review tuple
/// for a panel artifact and determine its canonical output path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelReviewIdentityV1 {
    /// Stable logical panel seat identity.
    pub(crate) seat_id: String,
    /// Exact logical generation routed for this artifact.
    pub(crate) generation: u32,
    /// Preallocated wake identity that delivered the prompt.
    pub(crate) wake_id: String,
    /// Attempt base commit fixing policy semantics.
    pub(crate) policy_base_sha: String,
}

/// Runtime-owned identity against which every review artifact is decoded.
///
/// The six base identity fields are immutable; panel routes add four more
/// immutable generation fields. `verdict` remains reviewer-owned but is
/// accepted only as one of `PASS|FAIL|BLOCKED` by the shared codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewContractExpectation {
    task_id: String,
    round: String,
    attempt_id: String,
    role: String,
    reviewer: String,
    reviewed_head: String,
    panel: Option<PanelReviewIdentityV1>,
}

impl ReviewContractExpectation {
    /// Construct and validate the immutable six-field legacy review tuple.
    /// Invalid task/attempt coupling, unsafe components, and non-full HEADs are
    /// rejected before any artifact path or reviewer prompt can consume it.
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
            panel: None,
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

    /// Construct the exact legacy tuple plus the four panel identity fields
    /// that must appear in frontmatter and the generation-scoped path.
    #[allow(clippy::too_many_arguments)]
    pub fn panel_exact(
        task_id: impl Into<String>,
        round: impl Into<String>,
        attempt_id: impl Into<String>,
        role: impl Into<String>,
        reviewer: impl Into<String>,
        reviewed_head: impl Into<String>,
        seat_id: impl Into<String>,
        generation: u32,
        wake_id: impl Into<String>,
        policy_base_sha: impl Into<String>,
    ) -> Result<Self> {
        let mut expectation =
            Self::exact(task_id, round, attempt_id, role, reviewer, reviewed_head)?;
        let panel = PanelReviewIdentityV1 {
            seat_id: seat_id.into(),
            generation,
            wake_id: wake_id.into(),
            policy_base_sha: policy_base_sha.into(),
        };
        if !safe_identity_component(&panel.seat_id)
            || panel.generation == 0
            || !safe_identity_component(&panel.wake_id)
        {
            bail!("panel review seat/generation/wake identity 非 canonical");
        }
        full_sha(&panel.policy_base_sha, "panel review policyBaseSha")?;
        expectation.panel = Some(panel);
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

    /// Return panel identity only for dynamic-pool expectations.
    pub(crate) fn panel_identity(&self) -> Option<&PanelReviewIdentityV1> {
        self.panel.as_ref()
    }

    pub(crate) fn artifact_relpath(&self) -> String {
        match &self.panel {
            Some(panel) => format!(
                "coordination/rounds/{}/reviews/{}-{}-g{}-{}.md",
                self.round, self.attempt_id, panel.seat_id, panel.generation, panel.wake_id,
            ),
            None => canonical_review_artifact_relpath(
                &self.round,
                &self.attempt_id,
                &self.role,
                &self.reviewer,
            ),
        }
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
    match expected.panel_identity() {
        Some(panel)
            if parsed.seat_id.as_deref() == Some(panel.seat_id.as_str())
                && parsed.generation == Some(panel.generation)
                && parsed.wake_id.as_deref() == Some(panel.wake_id.as_str())
                && parsed.policy_base_sha.as_deref() == Some(panel.policy_base_sha.as_str()) => {}
        Some(_) => bail!("panel review frontmatter seat/generation/wake/policyBase tuple 不匹配"),
        None if parsed.seat_id.is_none()
                && parsed.generation.is_none()
                && parsed.wake_id.is_none()
                && parsed.policy_base_sha.is_none() => {}
        None => bail!("legacy review frontmatter 不得注入 panel identity"),
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

fn parse_historical_round_ir_bytes(
    bytes: &[u8],
    label: &str,
) -> Result<crate::plan::RoundIr> {
    let text = std::str::from_utf8(bytes).with_context(|| format!("{label}: 非 UTF-8"))?;
    crate::plan::parse_signed_round_ir(text)
        .map_err(anyhow::Error::msg)
        .with_context(|| label.to_string())
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

fn substitutions_from_review_bindings(
    reviews: &[ReviewBinding],
) -> Result<Vec<ReviewSeatSubstitutionBinding>> {
    let mut substitutions = Vec::new();
    for binding in reviews {
        match (
            binding.substituted_role.as_deref(),
            binding.substituted_agent.as_deref(),
            binding.source_terminal_event_id.as_deref(),
            binding.delivery_event_id.as_deref(),
        ) {
            (None, None, None, _) => {}
            (Some(role), Some(from_agent), Some(source_terminal), Some(delivery_event))
                if binding.role == "nongate" =>
            {
                substitutions.push(ReviewSeatSubstitutionBinding {
                    role: role.to_string(),
                    from_agent: from_agent.to_string(),
                    to_agent: binding.reviewer.clone(),
                    reviewed_head: String::new(),
                    source_terminal_event_id: source_terminal.to_string(),
                    nongate_delivery_event_id: delivery_event.to_string(),
                });
            }
            _ => bail!("root review binding substitution fields 必须全有或全无"),
        }
    }
    Ok(substitutions)
}

fn signed_review_fallback<'a>(task: &'a crate::plan::IrTask, role: &str) -> Option<&'a str> {
    task.review_fallbacks
        .iter()
        .find(|fallback| fallback.role == role)
        .map(|fallback| fallback.fallback_agent.as_str())
}

fn validate_archived_quorum_binding_membership(
    reviews: &[ReviewBinding],
    evidence: &[EvidenceBinding],
    task: &crate::plan::IrTask,
    head_sha: &str,
) -> Result<Vec<ReviewSeatSubstitutionBinding>> {
    let policy = task
        .review_quorum
        .as_ref()
        .context("archived quorum validator 缺 reviewQuorum")?;
    let formal = task
        .required_reviews
        .iter()
        .map(|review| (review.role.as_str(), review))
        .collect::<BTreeMap<_, _>>();
    let nongate = task
        .nongate_seats
        .iter()
        .map(|seat| seat.agent.as_str())
        .collect::<BTreeSet<_>>();
    let mut agents = BTreeSet::new();
    let mut deliveries = BTreeSet::new();
    for binding in reviews {
        let delivery = binding
            .delivery_event_id
            .as_deref()
            .context("archived quorum review binding 缺 deliveryEventId")?;
        if !agents.insert(binding.reviewer.as_str()) || !deliveries.insert(delivery) {
            bail!("archived quorum review binding 重复 agent 或 deliveryEventId");
        }
        match binding.role.as_str() {
            "primary" | "secondary" => {
                let required = formal
                    .get(binding.role.as_str())
                    .context("archived quorum formal role 未签入 requiredReviews")?;
                if binding.reviewer != required.agent
                    && signed_review_fallback(task, binding.role.as_str())
                        != Some(binding.reviewer.as_str())
                {
                    bail!("archived quorum formal reviewer 未获 default/fallback 授权");
                }
            }
            "nongate" if nongate.contains(binding.reviewer.as_str()) => {}
            _ => bail!("archived quorum review binding role/agent 未获 signed policy 授权"),
        }
        if binding.verdict != "PASS" {
            bail!("archived root PASS quorum binding 含非 PASS finding");
        }
    }
    if reviews.len() < policy.minimum_substantive {
        let primary = task
            .required_reviews
            .iter()
            .filter(|required| required.role == "primary")
            .collect::<Vec<_>>();
        let [primary] = primary.as_slice() else {
            bail!("archived primary-pass exception 缺唯一 signed default primary");
        };
        if !task.primary_pass_alone_satisfies
            || reviews
                .iter()
                .filter(|binding| {
                    binding.role == "primary"
                        && binding.reviewer == primary.agent
                        && binding.verdict == "PASS"
                })
                .count()
                != 1
        {
            bail!("archived root PASS substantive review bindings 少于 signed minimum");
        }
    }
    let required_evidence_ids = task
        .required_evidence
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let bound_evidence_ids = evidence
        .iter()
        .map(|binding| binding.id.as_str())
        .collect::<BTreeSet<_>>();
    if required_evidence_ids.len() != task.required_evidence.len()
        || bound_evidence_ids.len() != evidence.len()
        || required_evidence_ids != bound_evidence_ids
    {
        bail!("archived quorum evidence bindings 未 exact 匹配 signed requiredEvidence");
    }
    let mut substitutions = substitutions_from_review_bindings(reviews)?;
    for substitution in &mut substitutions {
        substitution.reviewed_head = head_sha.to_string();
    }
    Ok(substitutions)
}

fn archived_formal_review_result(
    prefix: &[EventRecord],
    round: &str,
    task_id: &str,
    root_payload: &RootVerdictPayload,
    required: &crate::card::RequiredReview,
    reviewer: &str,
    terminal_counts: bool,
    binding: Option<&ReviewBinding>,
) -> Result<(ReviewResult, Option<(String, String, String)>)> {
    let delivery = review_delivery_event(
        prefix,
        "ReviewDelivered",
        round,
        task_id,
        &root_payload.attempt_id,
        &required.role,
        reviewer,
    )?;
    let request = current_formal_request(
        prefix,
        round,
        task_id,
        &root_payload.attempt_id,
        &required.role,
        reviewer,
        &root_payload.head_sha,
    )?;
    if let Some(binding) = binding {
        if binding.reviewer != reviewer {
            bail!("archived formal binding 未绑定当前 signed reviewer");
        }
        let delivery_event_id = binding
            .delivery_event_id
            .as_deref()
            .context("archived formal binding 缺 deliveryEventId")?;
        let delivery = delivery.context("archived formal binding 缺 exact ReviewDelivered")?;
        let request = request.context("archived formal binding 缺 exact ReviewRequested")?;
        let request_position = prefix
            .iter()
            .position(|event| event.event_id == request.event_id)
            .context("archived formal ReviewRequested 不在 root 前缀")?;
        let delivery_position = prefix
            .iter()
            .position(|event| event.event_id == delivery.event_id)
            .context("archived formal ReviewDelivered 不在 root 前缀")?;
        if delivery.event_id != delivery_event_id || request_position >= delivery_position {
            bail!("archived formal binding 未绑定 request→delivery exact order");
        }
        return Ok((
            ReviewResult {
                agent: reviewer.to_string(),
                role: required.role.clone(),
                attempt_id: root_payload.attempt_id.clone(),
                reviewed_head: root_payload.head_sha.clone(),
                delivery_event_id: delivery_event_id.to_string(),
                state: ReviewResultState::FormalPass,
            },
            None,
        ));
    }

    if delivery.is_some() {
        bail!("archived root 省略了已存在的 durable formal delivery");
    }
    let (delivery_event_id, state) = if let Some(request) = request {
        match managed_channel_terminal_after_request(prefix, round, task_id, reviewer, request)? {
            Some((terminal, ManagedChannelTerminalDisposition::SeatClosedOnly)) => (
                terminal.event_id.clone(),
                ReviewResultState::OperationalError,
            ),
            Some((terminal, ManagedChannelTerminalDisposition::FallbackEligible))
                if terminal_counts =>
            {
                (terminal.event_id.clone(), ReviewResultState::ChannelError)
            }
            _ => (request.event_id.clone(), ReviewResultState::Pending),
        }
    } else {
        (
            format!("pending:{}:{reviewer}", required.role),
            ReviewResultState::Pending,
        )
    };
    let formal_error = (state == ReviewResultState::ChannelError).then(|| {
        (
            required.role.clone(),
            reviewer.to_string(),
            delivery_event_id.clone(),
        )
    });
    Ok((
        ReviewResult {
            agent: reviewer.to_string(),
            role: required.role.clone(),
            attempt_id: root_payload.attempt_id.clone(),
            reviewed_head: root_payload.head_sha.clone(),
            delivery_event_id,
            state,
        },
        formal_error,
    ))
}

fn validate_durable_nongate_delivery_receipt_chain(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    root_payload: &RootVerdictPayload,
    seat: &crate::card::NongateSeat,
    delivery: &EventRecord,
) -> Result<()> {
    let wake_id = nongate_event_payload_str(delivery, "wakeId")
        .context("archived NongateReviewDelivered 缺 wakeId")?;
    let wake_event_id = nongate_event_payload_str(delivery, "wakeIssuedEventId")
        .context("archived NongateReviewDelivered 缺 wakeIssuedEventId")?;
    let lease_event_id = nongate_event_payload_str(delivery, "workspaceLeasedEventId")
        .context("archived NongateReviewDelivered 缺 workspaceLeasedEventId")?;
    if nongate_event_payload_str(delivery, "receiptState") != Some("answered") {
        bail!("substantive archived NongateReviewDelivered 必须绑定 answered receipt state");
    }
    let (lease_position, lease) = receipt_event(events, lease_event_id, "WorkspaceLeased")
        .map_err(anyhow::Error::msg)?;
    let (wake_position, wake) =
        receipt_event(events, wake_event_id, "WakeIssued").map_err(anyhow::Error::msg)?;
    let delivery_position = events
        .iter()
        .position(|event| event.event_id == delivery.event_id)
        .context("archived NongateReviewDelivered 不在 root 前缀")?;
    if lease_position >= wake_position || wake_position >= delivery_position {
        bail!("archived nongate durable order 不是 lease→wake→delivery");
    }
    for (kind, event) in [("WorkspaceLeased", lease), ("WakeIssued", wake)] {
        if event.actor != "runtime:orch"
            || event.round.as_deref() != Some(round)
            || event.task_id.as_deref() != Some(task_id)
            || nongate_event_payload_str(event, "attemptId")
                != Some(root_payload.attempt_id.as_str())
            || nongate_event_payload_str(event, "agent") != Some(seat.agent.as_str())
            || nongate_event_payload_str(event, "wakeId") != Some(wake_id)
        {
            bail!("archived nongate {kind} 未绑定 delivery exact tuple");
        }
    }
    if nongate_event_payload_str(lease, "role") != Some("nongate")
        || nongate_event_payload_str(lease, "reviewedHead")
            != Some(root_payload.head_sha.as_str())
    {
        bail!("archived nongate WorkspaceLeased 未绑定 fixed HEAD/role");
    }
    Ok(())
}

fn validate_archived_review_transition_chain(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    root_position: usize,
    root_payload: &RootVerdictPayload,
    task: &crate::plan::IrTask,
) -> Result<()> {
    let prefix = &events[..root_position];
    let policy = task
        .review_quorum
        .as_ref()
        .context("archived review transition 缺 signed reviewQuorum")?;
    let mut results = Vec::new();
    let mut formal_errors = Vec::new();
    for required in &task.required_reviews {
        let signed_fallback = signed_review_fallback(task, &required.role);
        let selections = prefix
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "ReviewFallbackSelected"
                    && event.round.as_deref() == Some(round)
                    && event.task_id.as_deref() == Some(task_id)
                    && nongate_event_payload_str(event, "attemptId")
                        == Some(root_payload.attempt_id.as_str())
                    && nongate_event_payload_str(event, "role") == Some(required.role.as_str())
            })
            .collect::<Vec<_>>();
        if selections.len() > 1 {
            bail!("archived formal seat 含重复 ReviewFallbackSelected");
        }
        let formal_bindings = root_payload
            .reviews
            .iter()
            .filter(|binding| binding.role == required.role)
            .collect::<Vec<_>>();
        if formal_bindings.len() > 1 {
            bail!("archived formal seat 含重复 root binding");
        }
        let formal_binding = formal_bindings.first().copied();
        let fallback_binding = root_payload.reviews.iter().find(|binding| {
            binding.role == required.role && signed_fallback == Some(binding.reviewer.as_str())
        });
        let substituted_fallback = root_payload.reviews.iter().find(|binding| {
            binding.substituted_role.as_deref() == Some(required.role.as_str())
                && signed_fallback == binding.substituted_agent.as_deref()
        });
        let Some((selection_position, selection)) = selections.first().copied() else {
            if fallback_binding.is_some() || substituted_fallback.is_some() {
                bail!("archived root 使用 fallback reviewer 但缺 ReviewFallbackSelected");
            }
            let (result, formal_error) = archived_formal_review_result(
                prefix,
                round,
                task_id,
                root_payload,
                required,
                &required.agent,
                signed_fallback.is_none(),
                formal_binding,
            )?;
            results.push(result);
            formal_errors.extend(formal_error);
            continue;
        };
        let target =
            signed_fallback.context("archived ReviewFallbackSelected 未获 signed fallbackAgent")?;
        let source_wake_id = nongate_event_payload_str(selection, "sourceWakeId")
            .context("archived ReviewFallbackSelected 缺 sourceWakeId")?;
        let terminal_event_id = nongate_event_payload_str(selection, "terminalEventId")
            .context("archived ReviewFallbackSelected 缺 terminalEventId")?;
        let target_wake_id = nongate_event_payload_str(selection, "targetWakeId")
            .context("archived ReviewFallbackSelected 缺 targetWakeId")?;
        if selection.actor != "runtime:orch"
            || nongate_event_payload_str(selection, "fromAgent") != Some(required.agent.as_str())
            || nongate_event_payload_str(selection, "toAgent") != Some(target)
            || nongate_event_payload_str(selection, "reviewedHead")
                != Some(root_payload.head_sha.as_str())
        {
            bail!("archived ReviewFallbackSelected 与 signed tuple 漂移");
        }
        let source_request = current_formal_request(
            prefix,
            round,
            task_id,
            &root_payload.attempt_id,
            &required.role,
            &required.agent,
            &root_payload.head_sha,
        )?
        .context("archived fallback source 缺 exact ReviewRequested")?;
        if nongate_event_payload_str(source_request, "wakeId") != Some(source_wake_id) {
            bail!("archived fallback sourceWakeId 未绑定 source ReviewRequested");
        }
        let (terminal, disposition) = managed_channel_terminal_after_request(
            prefix,
            round,
            task_id,
            &required.agent,
            source_request,
        )?
        .context("archived fallback source 缺可替换 terminal")?;
        if disposition != ManagedChannelTerminalDisposition::FallbackEligible {
            bail!("archived fallback source terminal 不授权替换");
        }
        let terminal_position = prefix
            .iter()
            .position(|event| event.event_id == terminal.event_id)
            .context("archived fallback terminal 不在 root 前缀")?;
        if terminal.event_id != terminal_event_id || terminal_position >= selection_position {
            bail!("archived fallback selection 未晚于 exact source terminal");
        }
        let target_request = current_formal_request(
            prefix,
            round,
            task_id,
            &root_payload.attempt_id,
            &required.role,
            target,
            &root_payload.head_sha,
        )?
        .context("archived fallback target 缺 ReviewRequested")?;
        if nongate_event_payload_str(target_request, "wakeId") != Some(target_wake_id)
            || nongate_event_payload_str(target_request, "reviewFallbackEventId")
                != Some(selection.event_id.as_str())
        {
            bail!("archived fallback target request 未反向绑定 selection/wake");
        }
        let (result, formal_error) = archived_formal_review_result(
            prefix,
            round,
            task_id,
            root_payload,
            required,
            target,
            true,
            formal_binding,
        )?;
        results.push(result);
        formal_errors.extend(formal_error);
    }

    if root_payload
        .reviews
        .iter()
        .filter(|binding| binding.role == "nongate")
        .any(|binding| {
            !task
                .nongate_seats
                .iter()
                .any(|seat| seat.agent == binding.reviewer)
        })
    {
        bail!("archived nongate binding 未获 signed nongateSeats 授权");
    }
    for seat in &task.nongate_seats {
        let delivery = review_delivery_event(
            prefix,
            "NongateReviewDelivered",
            round,
            task_id,
            &root_payload.attempt_id,
            "nongate",
            &seat.agent,
        )?;
        let bindings = root_payload
            .reviews
            .iter()
            .filter(|binding| binding.role == "nongate" && binding.reviewer == seat.agent)
            .collect::<Vec<_>>();
        if bindings.len() > 1 {
            bail!("archived nongate seat 含重复 root binding");
        }
        let binding = bindings.first().copied();
        let (Some(delivery), Some(binding)) = (delivery, binding) else {
            if delivery.is_some() || binding.is_some() {
                bail!("archived nongate delivery 与 root binding 必须同时存在");
            }
            continue;
        };
        let expectation = ReviewContractExpectation::exact(
            task_id,
            round,
            &root_payload.attempt_id,
            "nongate",
            &seat.agent,
            &root_payload.head_sha,
        )?;
        let delivery_event_id = binding
            .delivery_event_id
            .as_deref()
            .context("archived nongate binding 缺 deliveryEventId")?;
        let state = match binding.verdict.as_str() {
            "PASS" => ReviewResultState::NongatePass,
            "FAIL" => ReviewResultState::NongateFail,
            "BLOCKED" => ReviewResultState::Blocked,
            _ => bail!("archived nongate binding verdict 非闭合枚举"),
        };
        if binding.path != expectation.artifact_relpath()
            || delivery.event_id != delivery_event_id
            || delivery.actor != "runtime:orch"
            || delivery.task_id.as_deref() != Some(task_id)
            || delivery.round.as_deref() != Some(round)
            || nongate_event_payload_str(delivery, "attemptId")
                != Some(root_payload.attempt_id.as_str())
            || nongate_event_payload_str(delivery, "role") != Some("nongate")
            || nongate_event_payload_str(delivery, "agent") != Some(seat.agent.as_str())
            || nongate_event_payload_str(delivery, "reviewedHead")
                != Some(root_payload.head_sha.as_str())
            || nongate_event_payload_str(delivery, "path") != Some(binding.path.as_str())
            || nongate_event_payload_str(delivery, "sha256") != Some(binding.sha256.as_str())
            || delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bytes"))
                .and_then(serde_json::Value::as_u64)
                != Some(binding.bytes)
            || delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bodyLen"))
                .and_then(serde_json::Value::as_u64)
                .is_none_or(|body_len| body_len == 0)
            || nongate_event_payload_str(delivery, "verdict") != Some(binding.verdict.as_str())
        {
            bail!("archived NongateReviewDelivered 未绑定 root review bytes/identity");
        }
        validate_durable_nongate_delivery_receipt_chain(
            prefix,
            round,
            task_id,
            root_payload,
            seat,
            delivery,
        )?;
        results.push(ReviewResult {
            agent: seat.agent.clone(),
            role: "nongate".to_string(),
            attempt_id: root_payload.attempt_id.clone(),
            reviewed_head: root_payload.head_sha.clone(),
            delivery_event_id: delivery.event_id.clone(),
            state,
        });
    }

    let (decision, primary_pass_alone) =
        evaluate_review_quorum_internal(policy, Some(task), &results);
    if decision != ReviewQuorumDecision::Satisfied {
        bail!("archived root PASS review quorum 未满足: {decision:?}");
    }
    let nongate_passes = task
        .nongate_seats
        .iter()
        .filter_map(|seat| {
            root_payload.reviews.iter().find(|binding| {
                binding.role == "nongate"
                    && binding.reviewer == seat.agent
                    && binding.verdict == "PASS"
            })
        })
        .collect::<Vec<_>>();
    let required_substitutions = if primary_pass_alone {
        formal_errors.len().min(nongate_passes.len())
    } else {
        formal_errors.len()
    };
    let expected_substitutions = formal_errors
        .iter()
        .zip(nongate_passes.iter())
        .take(required_substitutions)
        .map(|((role, from_agent, terminal_event_id), binding)| {
            Ok(ReviewSeatSubstitutionBinding {
                role: role.clone(),
                from_agent: from_agent.clone(),
                to_agent: binding.reviewer.clone(),
                reviewed_head: String::new(),
                source_terminal_event_id: terminal_event_id.clone(),
                nongate_delivery_event_id: binding
                    .delivery_event_id
                    .clone()
                    .context("archived nongate PASS binding 缺 deliveryEventId")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if substitutions_from_review_bindings(&root_payload.reviews)? != expected_substitutions {
        bail!("archived root PASS substitution 未逐字绑定 signed seat/terminal/delivery 顺序");
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
            delivery_event_id: None,
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
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

fn doctor_review_binding_seat_role(binding: &ReviewBinding) -> Option<&str> {
    match (
        binding.role.as_str(),
        binding.substituted_role.as_deref(),
        binding.substituted_agent.as_deref(),
        binding.source_terminal_event_id.as_deref(),
        binding.delivery_event_id.as_deref(),
    ) {
        (role @ ("primary" | "secondary"), None, None, None, _) => Some(role),
        (
            "nongate",
            Some(role @ ("primary" | "secondary")),
            Some(agent),
            Some(terminal),
            Some(delivery),
        ) if !agent.is_empty() && !terminal.is_empty() && !delivery.is_empty() => Some(role),
        _ => None,
    }
}

#[cfg(test)]
mod doctor_review_binding_seat_tests {
    use super::*;

    fn binding(role: &str) -> ReviewBinding {
        ReviewBinding {
            path: format!("reviews/{role}.md"),
            role: role.to_string(),
            reviewer: "reviewer".to_string(),
            verdict: "PASS".to_string(),
            sha256: "a".repeat(64),
            bytes: 1,
            delivery_event_id: Some("delivery".to_string()),
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        }
    }

    #[test]
    fn doctor_maps_a_closed_nongate_substitution_to_the_formal_seat() {
        let mut review = binding("nongate");
        review.substituted_role = Some("primary".to_string());
        review.substituted_agent = Some("executor-opencode".to_string());
        review.source_terminal_event_id = Some("terminal".to_string());
        assert_eq!(doctor_review_binding_seat_role(&review), Some("primary"));
    }

    #[test]
    fn doctor_rejects_partial_or_unscoped_nongate_bindings() {
        let mut partial = binding("nongate");
        partial.substituted_role = Some("primary".to_string());
        assert_eq!(doctor_review_binding_seat_role(&partial), None);
        assert_eq!(doctor_review_binding_seat_role(&binding("nongate")), None);
    }
}

/// Audit every committed round ledger with the same V1 event-history
/// validator used by live append authority and archived record replay.
/// Returns `(ledgers_checked, v1_events_checked)` for doctor diagnostics.
pub fn audit_runtime_event_contracts(root: &Path) -> Result<(usize, usize)> {
    let main_sha = crate::gitx::rev_parse(root, "refs/heads/main")
        .context("runtime-event doctor cannot capture refs/heads/main")?;
    full_sha(&main_sha, "runtime-event doctor current main")?;
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
        .context("runtime-event doctor cannot enumerate round ledgers")?;
    if !output.status.success() {
        bail!(
            "runtime-event doctor cannot enumerate round ledgers ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !output.stdout.is_empty() && output.stdout.last() != Some(&0) {
        bail!("runtime-event doctor tree listing is not NUL terminated");
    }
    let mut ledgers = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            std::str::from_utf8(entry)
                .map(str::to_string)
                .context("runtime-event doctor ledger path is not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    ledgers
        .retain(|path| path.starts_with("coordination/rounds/") && path.ends_with("/events.jsonl"));
    ledgers.sort();
    let mut event_count = 0usize;
    for ledger_rel in &ledgers {
        let round = ledger_rel
            .strip_prefix("coordination/rounds/")
            .and_then(|value| value.strip_suffix("/events.jsonl"))
            .context("runtime-event doctor ledger path shape drift")?;
        if round.contains('/') {
            bail!("runtime-event doctor nested round path: {ledger_rel}");
        }
        let bytes = committed_regular_blob_bytes(
            root,
            &main_sha,
            ledger_rel,
            "runtime-event doctor ledger",
        )?;
        let events =
            parse_strict_committed_ledger(&bytes, &format!("runtime-event doctor {round} ledger"))?;
        crate::ledger::validate_runtime_event_history_v1_at_root(root, &events, round)?;
        for event in &events {
            let decoded = crate::ledger::decode_runtime_event_v1(event)?;
            if let Some(crate::ledger::RuntimeEventPayloadV1::ReviewSpoolPromoted(promotion)) =
                decoded.as_ref()
            {
                let artifact = committed_regular_blob_bytes(
                    root,
                    &main_sha,
                    &promotion.canonical_path,
                    "runtime-event doctor promoted review",
                )?;
                let (sha256, bytes) = sha_binding(&artifact);
                if sha256 != promotion.sha256 || bytes != promotion.bytes {
                    bail!(
                        "runtime-event doctor promoted review bytes drift: {}",
                        promotion.canonical_path
                    );
                }
                let task_id = event
                    .task_id
                    .as_deref()
                    .context("runtime-event doctor promotion 缺 taskId")?;
                let expectation = ReviewContractExpectation::panel_exact(
                    task_id,
                    round,
                    &promotion.attempt_id,
                    &promotion.role,
                    &promotion.agent,
                    &promotion.reviewed_head,
                    &promotion.seat_id,
                    promotion.generation,
                    &promotion.wake_id,
                    &promotion.policy_base_sha,
                )?;
                if expectation.artifact_relpath() != promotion.canonical_path {
                    bail!("runtime-event doctor promotion canonical path 漂移");
                }
                let checked = check_review_artifact_contract(&artifact, &expectation)?
                    .context("runtime-event doctor promoted review frontmatter incomplete")?;
                if checked.verdict() != promotion.verdict
                    || checked.substantive_body_len() as u64 != promotion.body_len
                {
                    bail!("runtime-event doctor promoted review verdict/bodyLen 漂移");
                }
            }
            if decoded.is_some() {
                event_count += 1;
            }
        }
    }
    if crate::gitx::rev_parse(root, "refs/heads/main")? != main_sha {
        bail!("runtime-event doctor main moved during committed-ledger audit");
    }
    Ok((ledgers.len(), event_count))
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
            let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
            let ir_bytes = committed_regular_blob_bytes(
                root,
                &payload.main_head_sha,
                &ir_rel,
                "doctor historical ROUND-IR",
            )?;
            let historical_ir = parse_historical_round_ir_bytes(
                &ir_bytes,
                "doctor historical ROUND-IR 非 canonical",
            )?;
            let historical_task = historical_ir
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .context("doctor historical ROUND-IR 缺 recorded task")?;
            if historical_ir.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
                for binding in payload.reviews.clone() {
                    let canonical_rel = crate::verify::canonical_review_artifact_relpath(
                        round,
                        &payload.attempt_id,
                        "review",
                        &binding.reviewer,
                    );
                    if binding.role != "review"
                        || binding.path != canonical_rel
                        || binding.reviewer == "local"
                        || binding.bytes == 0
                        || binding.delivery_event_id.is_none()
                    {
                        bail!("doctor schema 3 generic review binding 非 canonical");
                    }
                    full_sha256(&binding.sha256, "doctor generic review sha256")?;
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
                        report.findings.push(ReviewBindingAuditFinding {
                            level: ReviewBindingAuditLevel::Fail,
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
                continue;
            }
            if payload.reviews.is_empty() {
                bail!("doctor root PASS review bindings are empty");
            }
            let review_mode = review_contract_mode_for_attempt(
                root,
                round,
                &events,
                &task_id,
                &payload.attempt_id,
                historical_task,
            )?;
            let mut roles = BTreeSet::new();
            let mut reviewers = BTreeSet::new();
            let mut paths = BTreeSet::new();
            for binding in payload.reviews.clone() {
                full_sha256(&binding.sha256, "doctor bound review sha256")?;
                let seat_role = if review_mode == ReviewContractMode::Panel {
                    Some(binding.role.as_str())
                } else {
                    doctor_review_binding_seat_role(&binding)
                };
                let expected = if review_mode == ReviewContractMode::Panel {
                    let promotions = events
                        .iter()
                        .filter_map(|event| {
                            let Ok(Some(
                                crate::ledger::RuntimeEventPayloadV1::ReviewSpoolPromoted(
                                promotion,
                                ),
                            )) = crate::ledger::decode_runtime_event_v1(event)
                            else {
                                return None;
                            };
                            (promotion.canonical_path == binding.path
                                && promotion.agent == binding.reviewer
                                && promotion.role == binding.role)
                                .then_some(promotion)
                        })
                        .collect::<Vec<_>>();
                    let [promotion] = promotions.as_slice() else {
                        bail!("doctor panel binding 缺唯一 ReviewSpoolPromoted");
                    };
                    ReviewContractExpectation::panel_exact(
                        &task_id,
                        round,
                        &payload.attempt_id,
                        &binding.role,
                        &binding.reviewer,
                        &payload.head_sha,
                        &promotion.seat_id,
                        promotion.generation,
                        &promotion.wake_id,
                        &promotion.policy_base_sha,
                    )?
                } else {
                    ReviewContractExpectation::exact(
                        &task_id,
                        round,
                        &payload.attempt_id,
                        &binding.role,
                        &binding.reviewer,
                        &payload.head_sha,
                    )?
                };
                let canonical_rel = expected.artifact_relpath();
                if binding.path != canonical_rel
                    || seat_role.is_none()
                    || binding.verdict != "PASS"
                    || binding.reviewer == payload.implementer_agent
                    || binding.bytes == 0
                    || (review_mode != ReviewContractMode::Panel
                        && !roles.insert(seat_role.unwrap().to_string()))
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

fn committed_attempt_policy_binding(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    legacy_main_sha: &str,
) -> Result<binding::Binding> {
    let dispatch = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    if dispatch.attempt_id.as_deref() != Some(attempt_id) {
        bail!("root gate binding attempt 不是 durable current DispatchIssued");
    }
    match (dispatch.base_sha.as_deref(), dispatch.is_legacy) {
        (Some(policy_base_sha), _) => {
            full_sha(policy_base_sha, "root gate policyBaseSha")?;
            let binding_rel = "coordination/PROJECT-BINDING.yaml";
            if crate::gitx::tree_path_exists(root, policy_base_sha, binding_rel)? {
                if crate::round::contract_schema_from_events(events, round)?
                    == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
                {
                    let bytes = crate::gitx::show_bytes(root, policy_base_sha, binding_rel)?;
                    binding::validate_v3_binding_shape(&bytes)
                        .context("attempt-base schema 3 PROJECT-BINDING 非 closed shape")?;
                }
                return committed_binding(root, policy_base_sha)
                    .context("读取 attempt-base committed PROJECT-BINDING 失败");
            }
            if attempt_runtime_policy_active(
                root,
                round,
                events,
                task_id,
                attempt_id,
                "candidate-lanes-v1",
            )? {
                bail!("active candidate-lanes attempt base 缺 committed PROJECT-BINDING");
            }
            committed_binding(root, legacy_main_sha)
                .context("读取 pre-lane legacy expected-main PROJECT-BINDING 失败")
        }
        (None, Some(true)) => committed_binding(root, legacy_main_sha)
            .context("读取 legacy expected-main PROJECT-BINDING 失败"),
        (None, _) => bail!("modern root gate binding 缺 DispatchIssued.baseSha"),
    }
}

#[derive(Clone, Copy)]
enum CommittedLedgerMode {
    Exact,
    CanonicalStorageSuffix,
    CanonicalVerdictSuffix,
    CanonicalRootSuffix,
    CanonicalPostMergeSuffix,
}

fn runtime_event_allowed_in_committed_mode(
    event: &EventRecord,
    round: &str,
    mode: CommittedLedgerMode,
) -> Result<bool> {
    let Some(decoded) = crate::ledger::decode_runtime_event_v1(event)? else {
        return Ok(false);
    };
    crate::ledger::canonical_runtime_event_v1(event, round)?;
    let allowed = match decoded {
        crate::ledger::RuntimeEventPayloadV1::RuntimePolicyActivated(_)
        | crate::ledger::RuntimeEventPayloadV1::RuntimePolicyDeactivated(_) => false,
        crate::ledger::RuntimeEventPayloadV1::ReviewPanelSelected(_)
        | crate::ledger::RuntimeEventPayloadV1::ReviewSeatRouted(_)
        | crate::ledger::RuntimeEventPayloadV1::ReviewSeatTerminated(_)
        | crate::ledger::RuntimeEventPayloadV1::ReviewSpoolPromoted(_)
        | crate::ledger::RuntimeEventPayloadV1::ReviewPanelClosed(_)
        | crate::ledger::RuntimeEventPayloadV1::GateLaneEscalated(_) => {
            matches!(mode, CommittedLedgerMode::CanonicalVerdictSuffix)
        }
        crate::ledger::RuntimeEventPayloadV1::GateReuseMiss(payload) => {
            matches!(
                (payload.phase.as_str(), mode),
                ("root", CommittedLedgerMode::CanonicalVerdictSuffix)
                    | ("root", CommittedLedgerMode::CanonicalRootSuffix)
                    | ("trial", CommittedLedgerMode::CanonicalRootSuffix)
                    | ("root", CommittedLedgerMode::CanonicalPostMergeSuffix)
                    | ("trial", CommittedLedgerMode::CanonicalPostMergeSuffix)
                    | ("postmerge", CommittedLedgerMode::CanonicalPostMergeSuffix)
            )
        }
        crate::ledger::RuntimeEventPayloadV1::GateReused(payload) => match mode {
            CommittedLedgerMode::CanonicalVerdictSuffix
            | CommittedLedgerMode::CanonicalRootSuffix => payload.target_phase == "root",
            CommittedLedgerMode::CanonicalPostMergeSuffix => {
                matches!(payload.target_phase.as_str(), "root" | "postmerge")
            }
            CommittedLedgerMode::Exact | CommittedLedgerMode::CanonicalStorageSuffix => false,
        },
    };
    Ok(allowed)
}

/// Decode the exact ten-key GateExecuted payload shared by expected-main,
/// archive replay, and B310 gate-reuse authority.
pub(crate) fn canonical_gate_executed_payload<'a>(
    event: &'a EventRecord,
    round: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    const KEYS: &[&str] = &[
        "commandRef",
        "phase",
        "gateRunId",
        "exitCode",
        "durationMs",
        "subjectTreeSha",
        "logSha256",
        "logBytes",
        "toolchainDigest",
        "environmentDigest",
    ];
    if event.kind != "GateExecuted"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref().is_none_or(str::is_empty)
    {
        return None;
    }
    let payload = event.payload.as_ref()?.as_object()?;
    if payload.len() != KEYS.len() || !KEYS.iter().all(|key| payload.contains_key(*key)) {
        return None;
    }
    let command_ref = payload.get("commandRef")?.as_str()?;
    let phase = payload.get("phase")?.as_str()?;
    let gate_run_id = payload.get("gateRunId")?.as_str()?;
    let exit_code = payload.get("exitCode")?.as_i64()?;
    let subject_tree_sha = payload.get("subjectTreeSha")?.as_str()?;
    let log_sha256 = payload.get("logSha256")?.as_str()?;
    let toolchain_digest = payload.get("toolchainDigest")?.as_str()?;
    let environment_digest = payload.get("environmentDigest")?.as_str()?;
    if command_ref.is_empty()
        || !matches!(
            phase,
            "red-replay" | "collect" | "trial" | "root" | "postmerge" | "recovery"
        )
        || gate_run_id.parse::<ulid::Ulid>().is_err()
        || i32::try_from(exit_code).is_err()
        || payload.get("durationMs")?.as_u64().is_none()
        || full_sha(subject_tree_sha, "GateExecuted.subjectTreeSha").is_err()
        || payload.get("logBytes")?.as_u64().is_none()
        || full_sha256(log_sha256, "GateExecuted.logSha256").is_err()
        || full_sha256(toolchain_digest, "GateExecuted.toolchainDigest").is_err()
        || full_sha256(environment_digest, "GateExecuted.environmentDigest").is_err()
    {
        return None;
    }
    Some(payload)
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
        canonical_gate_executed_payload(event, round).is_some()
            || matches!(
                crate::ledger::decode_runtime_event_v1(event),
                Ok(Some(crate::ledger::RuntimeEventPayloadV1::GateReuseMiss(ref payload)))
                    if event.round.as_deref() == Some(round)
                        && event.task_id.as_deref() == Some(task_id)
                        && payload.phase == "root"
            )
    }) {
        CommittedLedgerMode::CanonicalVerdictSuffix
    } else if events.iter().any(|event| {
        crate::ledger::canonical_gate_storage_audit_event(event)
            .is_some_and(|audit| audit.round == round)
    }) {
        CommittedLedgerMode::CanonicalStorageSuffix
    } else {
        CommittedLedgerMode::Exact
    }
}

fn event_payload_str<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event.payload.as_ref()?.get(key)?.as_str()
}

fn event_payload_u32(event: &EventRecord, key: &str) -> Option<u32> {
    event.payload.as_ref()?.get(key)?.as_u64()?.try_into().ok()
}

/// Version of the complete-seal cleanup grammar admitted after the sealed
/// task's durable all-green `TaskRecorded` anchor.  Bumping this constant must
/// accompany any widening or narrowing of that replay boundary.
pub const COMPLETE_SEAL_TERMINAL_SUFFIX_CONTRACT_V1: u32 = 1;

fn managed_terminal_facts(
    event: &EventRecord,
    round: &str,
) -> Option<wake::ManagedWakeTerminationFacts> {
    if event.kind != "ManagedWakeTerminated"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || !canonical_event_extra(event)
    {
        return None;
    }
    let payload = event.payload.as_ref()?.as_object()?;
    let required_nonblank = |key: &str| {
        payload
            .get(key)?
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    };
    let wake_id = required_nonblank("wakeId")?;
    let agent = required_nonblank("agent")?;
    let completion_reason = required_nonblank("completionReason")?;
    let cancel_request_id = match payload.get("cancelRequestId")? {
        serde_json::Value::Null => None,
        serde_json::Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        _ => return None,
    };
    let signals = payload
        .get("signals")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    Some(wake::ManagedWakeTerminationFacts {
        wake_id,
        agent,
        completion_reason: Some(completion_reason),
        terminal_seen: payload.get("terminalSeen")?.as_bool()?,
        exited_naturally: payload.get("exitedNaturally")?.as_bool()?,
        hard_deadline_reached: payload.get("hardDeadlineReached")?.as_bool()?,
        cancel_request_id,
        signals,
        managed_scope_terminated: payload.get("managedScopeTerminated")?.as_bool()?,
        log_bytes_read: payload.get("logBytesRead")?.as_u64()?,
    })
}

fn canonical_managed_terminal_facts(
    event: &EventRecord,
    round: &str,
) -> Option<wake::ManagedWakeTerminationFacts> {
    let facts = managed_terminal_facts(event, round)?;
    let evidence = wake::SessionDeathEvidence::from_terminal_facts(&facts)?;
    (event_payload_str(event, "outcomeClass") == Some(evidence.outcome())).then_some(facts)
}

fn canonical_complete_seal_wake_extra(event: &EventRecord) -> bool {
    event.extra.iter().all(|(key, value)| match key.as_str() {
        "plannerWakeId" | "initiatorKind" | "invocationMode" => true,
        "modelWakeReservationId" => value.as_str().is_some_and(|reservation| {
            !reservation.trim().is_empty() && reservation.parse::<ulid::Ulid>().is_ok()
        }),
        _ => false,
    })
}

/// Authorize the only managed-wake cleanup batch that a replayed complete seal
/// may observe after its durable replay base.  During the first seal that base
/// is the historical expected-main plus its already accepted lifecycle suffix;
/// once `TaskRecorded` is committed, it is the current-main ledger byte prefix.
/// Authority comes from one same-round all-green record anchor plus one exact
/// earlier wake; an existing workspace lease additionally makes the recomputed
/// adjacent release mandatory.  Malformed identity, terminal facts,
/// duplicates, or extra suffix events fail closed instead of inheriting
/// authority from their event kinds.
pub fn canonical_complete_seal_managed_terminal_suffix_v1(
    prior: &[EventRecord],
    suffix: &[EventRecord],
    round: &str,
    sealed_task_id: &str,
) -> Result<bool> {
    if sealed_task_id.trim().is_empty()
        || !matches!(suffix.len(), 1 | 2)
        || suffix[0].kind != "ManagedWakeTerminated"
        || (suffix.len() == 2 && suffix[1].kind != "WorkspaceReleased")
    {
        return Ok(false);
    }
    let terminal = &suffix[0];
    let Some(facts) = canonical_managed_terminal_facts(terminal, round) else {
        return Ok(false);
    };

    let anchors = prior
        .iter()
        .filter(|event| {
            event.kind == "TaskRecorded"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(sealed_task_id)
                && event.payload.as_ref()
                    == Some(&serde_json::json!({"postMergeGates": "all-green"}))
                && canonical_event_extra(event)
        })
        .count();
    if anchors != 1 {
        return Ok(false);
    }

    let wakes = prior
        .iter()
        .filter(|event| {
            event.kind == "WakeIssued"
                && event_payload_str(event, "wakeId") == Some(facts.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    let [wake] = wakes.as_slice() else {
        return Ok(false);
    };
    if wake.actor != "runtime:orch"
        || wake.round.as_deref() != Some(round)
        || wake.task_id != terminal.task_id
        || event_payload_str(wake, "agent") != Some(facts.agent.as_str())
        || !canonical_complete_seal_wake_extra(wake)
        || prior.iter().any(|event| {
            event.kind == "ManagedWakeTerminated"
                && event_payload_str(event, "wakeId") == Some(facts.wake_id.as_str())
        })
    {
        return Ok(false);
    }

    let leases = prior
        .iter()
        .filter(|event| {
            event.kind == "WorkspaceLeased"
                && event_payload_str(event, "wakeId") == Some(facts.wake_id.as_str())
        })
        .collect::<Vec<_>>();
    match leases.as_slice() {
        [] => Ok(suffix.len() == 1),
        [lease]
            if lease.actor == "runtime:orch"
                && lease.round.as_deref() == Some(round)
                && canonical_event_extra(lease) =>
        {
            let Some(expected) =
                crate::sites::workspace_release_for_termination(prior, round, terminal)?
            else {
                return Ok(false);
            };
            let Some(actual) = suffix.get(1) else {
                return Ok(false);
            };
            Ok(actual.kind == expected.kind
                && actual.actor == expected.actor
                && actual.round == expected.round
                && actual.task_id == expected.task_id
                && actual.payload == expected.payload
                && canonical_event_extra(actual)
                && actual.extra == terminal.extra)
        }
        _ => Ok(false),
    }
}

/// Validate one or more consecutive cleanup-only managed terminal batches.
/// Each batch is independently bound to the same recorded task and extends
/// the visible prefix before the next batch is checked.
pub fn canonical_complete_seal_managed_terminal_suffixes_v1(
    prior: &[EventRecord],
    suffix: &[EventRecord],
    round: &str,
    sealed_task_id: &str,
) -> Result<bool> {
    if suffix.is_empty() {
        return Ok(true);
    }
    let mut visible = prior.to_vec();
    let mut position = 0usize;
    while position < suffix.len() {
        if suffix[position].kind != "ManagedWakeTerminated" {
            return Ok(false);
        }
        let end = if suffix
            .get(position + 1)
            .is_some_and(|event| event.kind == "WorkspaceReleased")
        {
            position + 2
        } else {
            position + 1
        };
        if !canonical_complete_seal_managed_terminal_suffix_v1(
            &visible,
            &suffix[position..end],
            round,
            sealed_task_id,
        )? {
            return Ok(false);
        }
        visible.extend_from_slice(&suffix[position..end]);
        position = end;
    }
    Ok(true)
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

#[derive(Debug)]
struct RecordedFrozenAuthorization {
    declarations: Vec<card::FrozenContractSupersession>,
    root_payload: RootVerdictPayload,
    verdict_event_id: String,
    plan_signed_off_event_id: String,
    task_card_sha256: String,
    merge_sha: String,
}

fn frozen_payload_declaration(
    payload: &FrozenContractSupersededPayload,
) -> card::FrozenContractSupersession {
    payload.signed_declaration()
}

/// Match the two signed formal review identities against their root bindings.
/// A durable nongate substitution represents the original formal tuple, not
/// the nongate reviewer's own role; partial substitution metadata is invalid.
pub(crate) fn declared_reviews_match_bindings(
    declared: &[card::RequiredReview],
    bindings: &[ReviewBinding],
) -> bool {
    let declared = declared
        .iter()
        .map(|review| (review.role.as_str(), review.agent.as_str()))
        .collect::<BTreeSet<_>>();
    if declared.len() != 2 || bindings.len() != 2 {
        return false;
    }
    let mut bound = BTreeSet::new();
    for review in bindings {
        if review.verdict != "PASS" {
            return false;
        }
        let identity = match (
            review.substituted_role.as_deref(),
            review.substituted_agent.as_deref(),
        ) {
            (None, None) => (review.role.as_str(), review.reviewer.as_str()),
            (Some(role), Some(agent))
                if review.role == "nongate"
                    && review.source_terminal_event_id.is_some()
                    && review.delivery_event_id.is_some() =>
            {
                (role, agent)
            }
            _ => return false,
        };
        if !bound.insert(identity) {
            return false;
        }
    }
    declared == bound
}

#[cfg(test)]
mod frozen_review_binding_tests {
    use super::*;

    fn binding(role: &str, reviewer: &str) -> ReviewBinding {
        ReviewBinding {
            path: format!("reviews/{role}-{reviewer}.md"),
            role: role.to_string(),
            reviewer: reviewer.to_string(),
            verdict: "PASS".to_string(),
            sha256: "a".repeat(64),
            bytes: 1,
            delivery_event_id: Some(format!("delivery-{role}")),
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        }
    }

    fn declared() -> Vec<card::RequiredReview> {
        vec![
            card::RequiredReview {
                role: "primary".to_string(),
                agent: "executor-opencode".to_string(),
            },
            card::RequiredReview {
                role: "secondary".to_string(),
                agent: "executor-pi".to_string(),
            },
        ]
    }

    #[test]
    fn frozen_reviews_accept_one_durable_nongate_substitution() {
        let mut nongate = binding("nongate", "executor-dsh");
        nongate.substituted_role = Some("primary".to_string());
        nongate.substituted_agent = Some("executor-opencode".to_string());
        nongate.source_terminal_event_id = Some("terminal-primary".to_string());
        assert!(declared_reviews_match_bindings(
            &declared(),
            &[binding("secondary", "executor-pi"), nongate]
        ));
    }

    #[test]
    fn frozen_reviews_reject_partial_or_wrong_substitution_identity() {
        let mut partial = binding("nongate", "executor-dsh");
        partial.substituted_role = Some("primary".to_string());
        assert!(!declared_reviews_match_bindings(
            &declared(),
            &[binding("secondary", "executor-pi"), partial]
        ));

        let mut wrong = binding("nongate", "executor-dsh");
        wrong.substituted_role = Some("primary".to_string());
        wrong.substituted_agent = Some("executor-other".to_string());
        assert!(!declared_reviews_match_bindings(
            &declared(),
            &[binding("secondary", "executor-pi"), wrong]
        ));
    }
}

fn recorded_frozen_authorization(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
    recorded_position: usize,
) -> Result<RecordedFrozenAuthorization> {
    let recorded = events
        .get(recorded_position)
        .context("frozen supersession TaskRecorded position 越界")?;
    if recorded.kind != "TaskRecorded"
        || recorded.actor != "runtime:orch"
        || recorded.round.as_deref() != Some(round)
        || recorded.task_id.as_deref() != Some(task_id)
        || !canonical_event_extra(recorded)
    {
        bail!("frozen supersession 未紧邻 canonical TaskRecorded");
    }
    let recorded_payload: TaskRecordedPayload = serde_json::from_value(
        recorded
            .payload
            .clone()
            .context("frozen supersession TaskRecorded 缺 payload")?,
    )
    .context("frozen supersession TaskRecorded payload 非 canonical")?;
    if recorded_payload.post_merge_gates != "all-green" {
        bail!("frozen supersession 只允许 post-green TaskRecorded");
    }

    let (merge_position, merge_payload) = events[..recorded_position]
        .iter()
        .enumerate()
        .rev()
        .find_map(|(position, event)| {
            if event.kind == "MergeExecuted"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
            {
                Some((position, event))
            } else {
                None
            }
        })
        .context("frozen supersession TaskRecorded 前缺 MergeExecuted")?;
    if merge_payload.actor != "reviewer:orch-runtime" || !canonical_event_extra(merge_payload) {
        bail!("frozen supersession MergeExecuted envelope 非 canonical");
    }
    let merged: MergeExecutedPayload = serde_json::from_value(
        merge_payload
            .payload
            .clone()
            .context("frozen supersession MergeExecuted 缺 payload")?,
    )
    .context("frozen supersession MergeExecuted payload 非 canonical")?;
    if merged.policy != "no-ff" {
        bail!("frozen supersession MergeExecuted policy 非 no-ff");
    }
    full_sha(&merged.merge_sha, "FrozenContractSuperseded.mergeSha")?;

    let (started_position, started) =
        final_recorded_merge_start(events, round, task_id, merge_position, recorded_position)?;
    let roots = events[..started_position]
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == started.verdict_event_id)
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        bail!("frozen supersession MergeStarted 未绑定唯一 root verdict");
    }
    let (root_position, root_event) = roots[0];
    if root_event.kind != "VerdictIssued"
        || root_event.actor != "verifier:root"
        || root_event.task_id.as_deref() != Some(task_id)
        || root_event.round.as_deref() != Some(round)
        || !canonical_event_extra(root_event)
    {
        bail!("frozen supersession root verdict envelope 非 canonical");
    }
    let root_payload = parse_root_payload(root_event)?;
    if root_payload.verdict != "PASS"
        || root_payload.reason.is_some()
        || root_payload.attempt_id != started.attempt_id
        || root_payload.attempt_no != started.attempt_no
        || root_payload.head_sha != started.head_sha
        || root_payload.main_head_sha != started.main_head_sha
        || root_payload.collect_completed_event_id != started.collect_completed_event_id
        || root_payload
            .reviews
            .iter()
            .any(|review| review.verdict != "PASS")
    {
        bail!("frozen supersession root PASS 未精确绑定 final MergeStarted");
    }

    let signoffs = crate::plan::matching_user_plan_signoff_positions(
        &events[..root_position],
        round,
        root_payload.ir_revision,
        &root_payload.validation_digest,
    )?;
    if signoffs.len() != 1 {
        bail!("frozen supersession 缺唯一 exact PlanSignedOff");
    }
    let plan_signed_off_event_id = events[signoffs[0]].event_id.clone();

    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let ir_bytes = committed_regular_blob_bytes(
        root,
        &root_payload.main_head_sha,
        &ir_rel,
        "frozen supersession historical ROUND-IR",
    )?;
    let historical_ir = parse_historical_round_ir_bytes(
        &ir_bytes,
        "frozen supersession historical ROUND-IR 非 canonical",
    )?;
    if historical_ir.round != round || historical_ir.revision != root_payload.ir_revision {
        bail!("frozen supersession historical ROUND-IR round/revision 不匹配");
    }
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let task_card_sha256 = historical_ir
        .source_bindings
        .task_cards
        .get(&card_rel)
        .with_context(|| format!("historical ROUND-IR 未绑定 supersession card: {card_rel}"))?
        .clone();
    let card_bytes = committed_regular_blob_bytes(
        root,
        &root_payload.main_head_sha,
        &card_rel,
        "frozen supersession historical task card",
    )?;
    let (actual_card_sha256, _) = sha_binding(&card_bytes);
    if actual_card_sha256 != task_card_sha256 {
        bail!("frozen supersession historical card digest 未绑定 ROUND-IR");
    }
    let card_text = std::str::from_utf8(&card_bytes)
        .context("frozen supersession historical task card 非 UTF-8")?;
    let mut declarations = card::parse(&card_rel, task_id, card_text)?
        .meta
        .frozen_contract_supersessions;
    declarations.sort_by(|left, right| left.target.cmp(&right.target));

    Ok(RecordedFrozenAuthorization {
        declarations,
        root_payload,
        verdict_event_id: root_event.event_id.clone(),
        plan_signed_off_event_id,
        task_card_sha256,
        merge_sha: merged.merge_sha,
    })
}

fn validate_one_frozen_supersession(
    root: &Path,
    round: &str,
    task_id: &str,
    event: &EventRecord,
    recorded: &EventRecord,
    declaration: &card::FrozenContractSupersession,
    authorization: &RecordedFrozenAuthorization,
    ledger_tip_sha: &str,
    validate_landed_anchor: bool,
) -> Result<FrozenContractSupersededPayload> {
    if event.kind != "FrozenContractSuperseded"
        || event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(task_id)
        || !canonical_event_extra(event)
    {
        bail!("FrozenContractSuperseded envelope 非 canonical runtime tuple");
    }
    let payload: FrozenContractSupersededPayload = serde_json::from_value(
        event
            .payload
            .clone()
            .context("FrozenContractSuperseded 缺 payload")?,
    )
    .context("FrozenContractSuperseded payload 非 closed typed schema")?;
    if frozen_payload_declaration(&payload) != *declaration
        || payload.task_recorded_event_id != recorded.event_id
        || payload.prior_main_sha != authorization.root_payload.main_head_sha
        || payload.candidate_sha != authorization.root_payload.head_sha
        || payload.merge_sha != authorization.merge_sha
        || payload.ir_revision != authorization.root_payload.ir_revision
        || payload.validation_digest != authorization.root_payload.validation_digest
        || payload.task_card_sha256 != authorization.task_card_sha256
        || payload.plan_signed_off_event_id != authorization.plan_signed_off_event_id
        || payload.verdict_event_id != authorization.verdict_event_id
        || payload.review_bindings != authorization.root_payload.reviews
        || !declared_reviews_match_bindings(&payload.reviews, &payload.review_bindings)
    {
        bail!("FrozenContractSuperseded 未精确绑定 signed declaration/root authorization");
    }
    for (value, label) in [
        (&payload.prior_main_sha, "priorMainSha"),
        (&payload.candidate_sha, "candidateSha"),
        (&payload.merge_sha, "mergeSha"),
        (&payload.effective_main_sha, "effectiveMainSha"),
    ] {
        full_sha(value, label)?;
    }
    full_sha(ledger_tip_sha, "FrozenContractSuperseded ledger tip")?;
    if !crate::gitx::is_ancestor(root, &payload.prior_main_sha, &payload.merge_sha)?
        || !crate::gitx::is_ancestor(root, &payload.candidate_sha, &payload.merge_sha)?
        || !crate::gitx::is_ancestor(root, &payload.merge_sha, &payload.effective_main_sha)?
        || !crate::gitx::is_ancestor(root, &payload.effective_main_sha, ledger_tip_sha)?
    {
        bail!("FrozenContractSuperseded candidate/merge/effective-main ancestry 非 canonical");
    }
    for (tree, label) in [
        (&payload.candidate_sha, "candidate"),
        (&payload.merge_sha, "merge"),
        (&payload.effective_main_sha, "effective main"),
    ] {
        let bytes = committed_regular_blob_bytes(root, tree, &payload.target, label)?;
        if sha_binding(&bytes).0 != payload.new_file_sha256 {
            bail!("FrozenContractSuperseded {label} target bytes != newFileSha256");
        }
    }
    if validate_landed_anchor {
        crate::oracle::validate_frozen_contract_declaration(
            root,
            declaration,
            &payload.prior_main_sha,
            &payload.effective_main_sha,
        )
        .context("FrozenContractSuperseded 四锚点/字节授权失败")?;
    }
    Ok(payload)
}

/// Validate one durable supersession as a signed post-green delta without
/// recursively asking the landed oracle to validate its own anchor chain.
/// The caller (oracle replay) supplies the immutable main tree whose ledger is
/// being projected and performs the chain/fork comparison after this returns.
pub fn validate_frozen_contract_supersession_delta_for_replay(
    root: &Path,
    main_oid: &str,
    events: &[EventRecord],
    position: usize,
) -> Result<FrozenContractSupersededPayload> {
    full_sha(main_oid, "frozen replay main")?;
    let event = events
        .get(position)
        .context("FrozenContractSuperseded replay position 越界")?;
    if event.kind != "FrozenContractSuperseded" {
        bail!("frozen replay position 不是 FrozenContractSuperseded");
    }
    let round = event
        .round
        .as_deref()
        .context("frozen replay event 缺 round")?;
    let task_id = event
        .task_id
        .as_deref()
        .context("frozen replay event 缺 taskId")?;
    let first_frozen = (0..=position)
        .rev()
        .find(|candidate| {
            *candidate == 0
                || events[*candidate - 1].kind != "FrozenContractSuperseded"
                || events[*candidate - 1].round.as_deref() != Some(round)
                || events[*candidate - 1].task_id.as_deref() != Some(task_id)
        })
        .context("frozen replay 无法定位 group 起点")?;
    let recorded_position = first_frozen
        .checked_sub(1)
        .context("FrozenContractSuperseded group 前缺 TaskRecorded")?;
    let authorization =
        recorded_frozen_authorization(root, round, task_id, events, recorded_position)?;
    let group_len = events[first_frozen..]
        .iter()
        .take_while(|candidate| {
            candidate.kind == "FrozenContractSuperseded"
                && candidate.round.as_deref() == Some(round)
                && candidate.task_id.as_deref() == Some(task_id)
        })
        .count();
    if group_len != authorization.declarations.len() {
        bail!("frozen replay group 与 signed declarations 数量不一致");
    }
    let declaration_index = position - first_frozen;
    let declaration = authorization
        .declarations
        .get(declaration_index)
        .context("frozen replay declaration index 越界")?;
    validate_one_frozen_supersession(
        root,
        round,
        task_id,
        event,
        &events[recorded_position],
        declaration,
        &authorization,
        main_oid,
        false,
    )
}

/// Validate every new TaskRecorded supersession group in one ordered suffix.
/// A group is inseparable from its TaskRecorded: declarations cannot be
/// omitted, delayed, duplicated, forked, or reordered into a later append.
fn validate_frozen_contract_supersession_suffix(
    root: &Path,
    round: &str,
    prior: &[EventRecord],
    suffix: &[EventRecord],
) -> Result<BTreeSet<String>> {
    if crate::round::contract_schema_from_events(prior, round)?
        == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        if suffix
            .iter()
            .any(|event| event.kind == "FrozenContractSuperseded")
        {
            bail!("schema 3 record suffix 不得产生 FrozenContractSuperseded");
        }
        return Ok(BTreeSet::new());
    }
    let ledger_tip_sha = crate::gitx::rev_parse(root, "main")?;
    let mut all = prior.to_vec();
    all.extend_from_slice(suffix);
    let base = prior.len();
    let mut accepted = BTreeSet::new();
    let mut index = 0usize;
    while index < suffix.len() {
        let event = &suffix[index];
        if event.kind == "FrozenContractSuperseded" {
            bail!("FrozenContractSuperseded 必须紧跟同批 canonical TaskRecorded");
        }
        if event.kind != "TaskRecorded" {
            index += 1;
            continue;
        }
        let task_id = event
            .task_id
            .as_deref()
            .context("TaskRecorded supersession group 缺 taskId")?;
        let authorization =
            recorded_frozen_authorization(root, round, task_id, &all, base + index)?;
        let mut end = index + 1;
        while end < suffix.len()
            && suffix[end].kind == "FrozenContractSuperseded"
            && suffix[end].task_id.as_deref() == Some(task_id)
            && suffix[end].round.as_deref() == Some(round)
        {
            end += 1;
        }
        let group = &suffix[index + 1..end];
        if group.len() != authorization.declarations.len() {
            bail!(
                "TaskRecorded supersession group 数量不匹配: task={task_id} declared={} effective={}",
                authorization.declarations.len(),
                group.len()
            );
        }
        for (candidate, declaration) in group.iter().zip(&authorization.declarations) {
            let payload = validate_one_frozen_supersession(
                root,
                round,
                task_id,
                candidate,
                event,
                declaration,
                &authorization,
                &ledger_tip_sha,
                true,
            )?;
            if payload.target != declaration.target || !accepted.insert(candidate.event_id.clone())
            {
                bail!("FrozenContractSuperseded target/eventId 重复或倒序");
            }
        }
        index = end;
    }
    Ok(accepted)
}

fn is_record_gate_relaxed(event: &EventRecord) -> bool {
    event.kind == "EscalationRaised"
        && event
            .payload
            .as_ref()
            .and_then(|payload| payload.get("stage"))
            .and_then(serde_json::Value::as_str)
            == Some("RecordGateRelaxed")
}

fn same_site_retirement_fact(actual: &EventRecord, expected: &EventRecord) -> bool {
    actual.kind == expected.kind
        && actual.actor == expected.actor
        && actual.round == expected.round
        && actual.task_id == expected.task_id
        && actual.payload == expected.payload
        && canonical_event_extra(actual)
}

fn validate_record_gate_relaxed(
    root: &Path,
    round: &str,
    task_id: &str,
    event: &EventRecord,
    merge_sha: &str,
    effective_main_sha: &str,
) -> Result<()> {
    if !is_record_gate_relaxed(event)
        || event.actor != "reviewer:orch-runtime"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref() != Some(task_id)
        || !canonical_event_extra(event)
    {
        bail!("FrozenContractSuperseded batch 末项非 canonical RecordGateRelaxed");
    }
    let relaxed: RecordGateRelaxedPayload = serde_json::from_value(
        event
            .payload
            .clone()
            .context("RecordGateRelaxed 缺 payload")?,
    )
    .context("RecordGateRelaxed payload 非 closed schema")?;
    if relaxed.stage != "RecordGateRelaxed"
        || relaxed.merge_sha != merge_sha
        || relaxed.tip_sha != effective_main_sha
        || relaxed.reason.trim().is_empty()
        || relaxed.files != crate::gitx::diff_names(root, merge_sha, effective_main_sha)?
    {
        bail!("RecordGateRelaxed 未精确绑定 supersession merge/effective-main/diff");
    }
    Ok(())
}

fn validate_schema3_record_relaxation_point(
    current_main: Option<&str>,
    merge_sha: &str,
    relaxation_tip: Option<&str>,
) -> Result<()> {
    match (current_main, relaxation_tip) {
        (None, None) => Ok(()),
        (None, Some(tip)) if tip != merge_sha => Ok(()),
        (Some(current), None) if current == merge_sha => Ok(()),
        (Some(current), Some(tip)) if current != merge_sha && tip == current => Ok(()),
        (Some(_), None) => {
            bail!("schema 3 non-mergeSha record point 缺 RecordGateRelaxed")
        }
        (Some(current), Some(_)) if current == merge_sha => {
            bail!("schema 3 mergeSha record point 不得发 RecordGateRelaxed")
        }
        (Some(_), Some(_)) => bail!("schema 3 RecordGateRelaxed tipSha 未绑定 current main"),
        (None, Some(_)) => bail!("schema 3 historical RecordGateRelaxed tipSha 等于 mergeSha"),
    }
}

#[cfg(test)]
mod schema3_record_relaxation_tests {
    use super::validate_schema3_record_relaxation_point;

    #[test]
    fn current_tip_relaxation_is_exact_iff_and_historical_replay_stays_portable() {
        let merge = "a".repeat(40);
        let tip = "b".repeat(40);
        let other = "c".repeat(40);

        validate_schema3_record_relaxation_point(None, &merge, None).unwrap();
        validate_schema3_record_relaxation_point(None, &merge, Some(&tip)).unwrap();
        validate_schema3_record_relaxation_point(Some(&merge), &merge, None).unwrap();
        validate_schema3_record_relaxation_point(Some(&tip), &merge, Some(&tip)).unwrap();

        assert!(validate_schema3_record_relaxation_point(Some(&tip), &merge, None).is_err());
        assert!(
            validate_schema3_record_relaxation_point(Some(&merge), &merge, Some(&tip)).is_err()
        );
        assert!(
            validate_schema3_record_relaxation_point(Some(&tip), &merge, Some(&other)).is_err()
        );
        assert!(validate_schema3_record_relaxation_point(None, &merge, Some(&merge)).is_err());
    }
}

/// Validate complete post-green record groups, including the exact retirement
/// set and the iff relaxation tail.  The weaker Frozen-only validator remains
/// useful while the emitter is still constructing its batch; admission and
/// committed-suffix verification must use this complete form.
fn validate_canonical_recorded_batches(
    root: &Path,
    round: &str,
    prior: &[EventRecord],
    suffix: &[EventRecord],
    require_current_tip: bool,
) -> Result<BTreeSet<String>> {
    let accepted = validate_frozen_contract_supersession_suffix(root, round, prior, suffix)?;
    let schema3 = crate::round::contract_schema_from_events(prior, round)?
        == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION);
    let mut all = prior.to_vec();
    all.extend_from_slice(suffix);
    let base = prior.len();
    let mut consumed = BTreeSet::new();
    let mut index = 0usize;
    while index < suffix.len() {
        let event = &suffix[index];
        if event.kind != "TaskRecorded" {
            if event.kind == "SiteRetired" || is_record_gate_relaxed(event) {
                bail!("post-merge retirement/relaxation 脱离 canonical TaskRecorded group");
            }
            index += 1;
            continue;
        }
        let task_id = event
            .task_id
            .as_deref()
            .context("canonical TaskRecorded group 缺 taskId")?;
        if schema3 {
            let root_payload = all[..base + index]
                .iter()
                .rev()
                .filter(|candidate| {
                    candidate.kind == "VerdictIssued"
                        && candidate.actor == "verifier:root"
                        && candidate.round.as_deref() == Some(round)
                        && candidate.task_id.as_deref() == Some(task_id)
                })
                .map(parse_root_payload)
                .find_map(|result| match result {
                    Ok(payload) if payload.verdict == "PASS" => Some(Ok(payload)),
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .transpose()?
                .context("schema 3 TaskRecorded 缺前置 root PASS signature")?;
            let ir = crate::plan::load_round_ir(root, round)?;
            let has_seeds = ir
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .with_context(|| format!("schema 3 TaskRecorded task {task_id} 不在 ROUND-IR"))?
                .has_seeds;
            crate::oracle::validate_v3_relocation_history(
                &all,
                round,
                task_id,
                root_payload.ir_revision,
                &root_payload.validation_digest,
                true,
                has_seeds,
            )?;
            let payload: TaskRecordedPayload = serde_json::from_value(
                event
                    .payload
                    .clone()
                    .context("schema 3 TaskRecorded 缺 payload")?,
            )?;
            if event.actor != "runtime:orch"
                || event.round.as_deref() != Some(round)
                || payload.post_merge_gates != "all-green"
            {
                bail!("schema 3 TaskRecorded envelope/payload 非 canonical");
            }
            let history = prior
                .iter()
                .cloned()
                .chain(suffix[..index].iter().cloned())
                .collect::<Vec<_>>();
            let expected_sites =
                crate::sites::retire_task_sites(&history, task_id, &event.event_id);
            let sites_end = index + 1 + expected_sites.len();
            if sites_end > suffix.len()
                || !suffix[index + 1..sites_end]
                    .iter()
                    .zip(&expected_sites)
                    .all(|(actual, expected)| same_site_retirement_fact(actual, expected))
                || suffix
                    .get(sites_end)
                    .is_some_and(|candidate| candidate.kind == "FrozenContractSuperseded")
            {
                bail!("schema 3 TaskRecorded SiteRetired group 非 canonical");
            }
            let recorded_relaxation = suffix
                .get(sites_end)
                .filter(|event| is_record_gate_relaxed(event));
            let current_main = require_current_tip
                .then(|| crate::gitx::rev_parse(root, "main"))
                .transpose()?;
            if recorded_relaxation.is_none() && current_main.is_none() {
                index = sites_end;
                continue;
            }

            let merge_events = history
                .iter()
                .filter(|candidate| {
                    candidate.kind == "MergeExecuted"
                        && candidate.actor == "reviewer:orch-runtime"
                        && candidate.round.as_deref() == Some(round)
                        && candidate.task_id.as_deref() == Some(task_id)
                })
                .collect::<Vec<_>>();
            let [merge_event] = merge_events.as_slice() else {
                bail!("schema 3 TaskRecorded relaxation 缺唯一 MergeExecuted");
            };
            let merged: MergeExecutedPayload = serde_json::from_value(
                merge_event
                    .payload
                    .clone()
                    .context("schema 3 TaskRecorded MergeExecuted 缺 payload")?,
            )?;
            if merged.policy != "no-ff" {
                bail!("schema 3 TaskRecorded MergeExecuted policy 非 no-ff");
            }
            full_sha(&merged.merge_sha, "schema 3 TaskRecorded mergeSha")?;

            let mut end = sites_end;
            let relaxation_payload = recorded_relaxation
                .map(|relaxed| {
                    serde_json::from_value::<RecordGateRelaxedPayload>(
                        relaxed
                            .payload
                            .clone()
                            .context("schema 3 RecordGateRelaxed 缺 payload")?,
                    )
                    .map_err(anyhow::Error::from)
                })
                .transpose()?;
            validate_schema3_record_relaxation_point(
                current_main.as_deref(),
                &merged.merge_sha,
                relaxation_payload.as_ref().map(|payload| payload.tip_sha.as_str()),
            )?;
            if let (Some(relaxed), Some(payload)) = (recorded_relaxation, relaxation_payload) {
                validate_record_gate_relaxed(
                    root,
                    round,
                    task_id,
                    relaxed,
                    &merged.merge_sha,
                    &payload.tip_sha,
                )?;
                end += 1;
            }
            index = end;
            continue;
        }
        let authorization =
            recorded_frozen_authorization(root, round, task_id, &all, base + index)?;
        let frozen_end = index + 1 + authorization.declarations.len();
        if frozen_end > suffix.len() {
            bail!("canonical TaskRecorded group 截断 FrozenContractSuperseded");
        }
        for frozen in &suffix[index + 1..frozen_end] {
            if frozen.kind != "FrozenContractSuperseded" || !accepted.contains(&frozen.event_id) {
                bail!("canonical TaskRecorded group FrozenContractSuperseded 漏项/错序");
            }
            consumed.insert(frozen.event_id.clone());
        }

        let history = prior
            .iter()
            .cloned()
            .chain(suffix[..index].iter().cloned())
            .collect::<Vec<_>>();
        let expected_sites = crate::sites::retire_task_sites(&history, task_id, &event.event_id);
        let sites_end = frozen_end + expected_sites.len();
        if sites_end > suffix.len()
            || !suffix[frozen_end..sites_end]
                .iter()
                .zip(&expected_sites)
                .all(|(actual, expected)| same_site_retirement_fact(actual, expected))
        {
            bail!("canonical TaskRecorded group SiteRetired 集合/顺序不完整");
        }

        let effective_main_sha = if let Some(first) = suffix
            .get(index + 1)
            .filter(|event| event.kind == "FrozenContractSuperseded")
        {
            let payload: FrozenContractSupersededPayload = serde_json::from_value(
                first
                    .payload
                    .clone()
                    .context("FrozenContractSuperseded 缺 payload")?,
            )?;
            payload.effective_main_sha
        } else if suffix.get(sites_end).is_some_and(is_record_gate_relaxed) {
            let payload: RecordGateRelaxedPayload = serde_json::from_value(
                suffix[sites_end]
                    .payload
                    .clone()
                    .context("RecordGateRelaxed 缺 payload")?,
            )?;
            payload.tip_sha
        } else {
            // Empty historical cards retain the ordinary record-recovery
            // anchor at MergeExecuted.  Only an emitted Frozen delta or an
            // explicit RecordGateRelaxed fact moves the effective point.
            authorization.merge_sha.clone()
        };
        let recovery_prefix = require_current_tip
            && index > 0
            && suffix[index - 1].kind == "MergeExecuted"
            && suffix[index - 1].round.as_deref() == Some(round)
            && suffix[index - 1].task_id.as_deref() == Some(task_id);
        let recorded_relaxation = suffix.get(sites_end).is_some_and(is_record_gate_relaxed);
        let needs_relaxation = recovery_prefix
            || effective_main_sha != authorization.merge_sha
            || (!require_current_tip && recorded_relaxation);
        let mut end = sites_end;
        if needs_relaxation {
            let relaxed = suffix
                .get(end)
                .context("non-mergeSha record point 缺 RecordGateRelaxed")?;
            validate_record_gate_relaxed(
                root,
                round,
                task_id,
                relaxed,
                &authorization.merge_sha,
                &effective_main_sha,
            )?;
            end += 1;
        } else if suffix.get(end).is_some_and(is_record_gate_relaxed) {
            bail!("mergeSha record point 不得发 RecordGateRelaxed");
        }
        index = end;
    }
    if consumed != accepted {
        bail!("canonical post-merge suffix 含未归组 FrozenContractSuperseded");
    }
    Ok(accepted)
}

/// Load the exact historical card bound by root authorization. The live
/// post-merge tree may already contain the new
/// target bytes before the durable Effective event exists, so re-deriving the
/// active IR here would make the first real supersession self-poison.
pub(crate) fn frozen_contract_card_from_record_authorization(
    root: &Path,
    round: &str,
    task_id: &str,
    authorization: &RootRecordAuthorization,
) -> Result<card::Card> {
    full_sha(
        &authorization.expected_main_sha,
        "frozen authorization expected main",
    )?;
    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let ir_bytes = committed_regular_blob_bytes(
        root,
        &authorization.expected_main_sha,
        &ir_rel,
        "frozen authorization historical ROUND-IR",
    )?;
    let historical_ir = parse_historical_round_ir_bytes(
        &ir_bytes,
        "frozen authorization historical ROUND-IR 非 canonical",
    )?;
    if historical_ir.round != round
        || historical_ir.revision != authorization.ir_revision
        || crate::plan::validation_digest(&historical_ir) != authorization.validation_digest
    {
        bail!("frozen authorization historical ROUND-IR identity/digest 漂移");
    }
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    if historical_ir.source_bindings.task_cards.get(&card_rel)
        != Some(&authorization.task_card_sha256)
    {
        bail!("frozen authorization historical card digest 未绑定 ROUND-IR/root");
    }
    let card_bytes = committed_regular_blob_bytes(
        root,
        &authorization.expected_main_sha,
        &card_rel,
        "frozen authorization historical task card",
    )?;
    let (actual_card_sha256, _) = sha_binding(&card_bytes);
    if actual_card_sha256 != authorization.task_card_sha256 {
        bail!("frozen authorization historical card bytes 漂移");
    }
    let card_text = std::str::from_utf8(&card_bytes)
        .context("frozen authorization historical task card 非 UTF-8")?;
    card::parse(&card_rel, task_id, card_text)
}

/// Rebuild the pending frozen-record context from committed root/merge facts.
/// This replay intentionally excludes mutable nongate receipts: the exact
/// `NongateReviewDelivered` and substitution events, committed artifacts and
/// root payload already carry the durable authorization needed by detached
/// post-merge gates.
pub(crate) fn pending_frozen_record_context_from_committed_main(
    root: &Path,
    effective_main_sha: &str,
    events: &[EventRecord],
) -> Result<Option<(RootRecordAuthorization, card::Card)>> {
    let Some(barrier) = crate::ledger::active_merge_barrier(events) else {
        return Ok(None);
    };
    if !barrier.merge_executed {
        return Ok(None);
    }
    full_sha(effective_main_sha, "pending frozen effective main")?;
    let round = barrier.round.as_str();
    let task_id = barrier.task_id.as_str();
    if crate::round::contract_schema_from_events(events, round)?
        == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        let root_payload = events
            .iter()
            .rev()
            .filter(|candidate| {
                candidate.kind == "VerdictIssued"
                    && candidate.actor == "verifier:root"
                    && candidate.round.as_deref() == Some(round)
                    && candidate.task_id.as_deref() == Some(task_id)
            })
            .map(parse_root_payload)
            .find_map(|result| match result {
                Ok(payload) if payload.verdict == "PASS" => Some(Ok(payload)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .transpose()?
            .context("pending schema 3 record 缺 root PASS signature")?;
        let ir = crate::plan::load_round_ir(root, round)?;
        let has_seeds = ir
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .with_context(|| format!("pending schema 3 task {task_id} 不在 ROUND-IR"))?
            .has_seeds;
        crate::oracle::validate_v3_relocation_history(
            events,
            round,
            task_id,
            root_payload.ir_revision,
            &root_payload.validation_digest,
            false,
            has_seeds,
        )?;
        return Ok(None);
    }
    if events.iter().any(|event| {
        event.kind == "TaskRecorded"
            && event.task_id.as_deref() == Some(task_id)
            && event.round.as_deref() == Some(round)
    }) {
        bail!("pending frozen context 不得包含 TaskRecorded");
    }

    let merges = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "MergeExecuted" && event.task_id.as_deref() == Some(task_id)
        })
        .collect::<Vec<_>>();
    let [(merge_position, merge_event)] = merges.as_slice() else {
        bail!("pending frozen context requires one MergeExecuted");
    };
    if merge_event.actor != "reviewer:orch-runtime"
        || merge_event.round.as_deref() != Some(round)
        || !canonical_event_extra(merge_event)
    {
        bail!("pending frozen MergeExecuted envelope 非 canonical");
    }
    let merged: MergeExecutedPayload = serde_json::from_value(
        merge_event
            .payload
            .clone()
            .context("pending frozen MergeExecuted 缺 payload")?,
    )
    .context("pending frozen MergeExecuted payload 非 canonical")?;
    if merged.policy != "no-ff" {
        bail!("pending frozen MergeExecuted policy 非 no-ff");
    }
    full_sha(&merged.merge_sha, "pending frozen mergeSha")?;
    if !crate::gitx::is_ancestor(root, &merged.merge_sha, effective_main_sha)? {
        bail!("pending frozen merge is not an effective-main ancestor");
    }

    let (started_position, started) =
        final_recorded_merge_start(events, round, task_id, *merge_position, events.len())?;
    let roots = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == started.verdict_event_id)
        .collect::<Vec<_>>();
    let [(root_position, root_event)] = roots.as_slice() else {
        bail!("pending frozen MergeStarted 缺唯一 root verdict");
    };
    if *root_position >= started_position
        || root_event.kind != "VerdictIssued"
        || root_event.actor != "verifier:root"
        || root_event.task_id.as_deref() != Some(task_id)
        || root_event.round.as_deref() != Some(round)
        || !canonical_event_extra(root_event)
    {
        bail!("pending frozen root verdict envelope/order 非 canonical");
    }
    let payload = parse_root_payload(root_event)?;
    if payload.verdict != "PASS"
        || payload.reason.is_some()
        || payload.attempt_id != started.attempt_id
        || payload.attempt_no != started.attempt_no
        || payload.head_sha != started.head_sha
        || payload.main_head_sha != started.main_head_sha
        || payload.collect_completed_event_id != started.collect_completed_event_id
        || payload.gates.is_empty()
        || payload.gates.iter().any(|gate| gate.exit_code != 0)
    {
        bail!("pending frozen root PASS 未绑定 MergeStarted");
    }

    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let ir_bytes = committed_regular_blob_bytes(
        root,
        &payload.main_head_sha,
        &ir_rel,
        "pending frozen historical ROUND-IR",
    )?;
    let historical_ir = parse_historical_round_ir_bytes(
        &ir_bytes,
        "pending frozen historical ROUND-IR 非 canonical",
    )?;
    if historical_ir.round != round
        || historical_ir.revision != payload.ir_revision
        || crate::plan::validation_digest(&historical_ir) != payload.validation_digest
        || historical_ir.verification.mode != "root-manual-fixed-head"
    {
        bail!("pending frozen historical ROUND-IR identity/digest 漂移");
    }
    let historical_task = historical_ir
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .context("pending frozen historical ROUND-IR 缺 task")?;
    if payload.bootstrap_pre_signoff_attempt != historical_task.bootstrap_pre_signoff_attempt {
        bail!("pending frozen bootstrap identity 漂移");
    }
    let signoffs = crate::plan::matching_user_plan_signoff_positions(
        &events[..*root_position],
        round,
        payload.ir_revision,
        &payload.validation_digest,
    )?;
    if signoffs.len() != 1 {
        bail!("pending frozen context 缺唯一 PlanSignedOff");
    }
    let plan_signed_off_event_id = events[signoffs[0]].event_id.clone();
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let task_card_sha256 = historical_ir
        .source_bindings
        .task_cards
        .get(&card_rel)
        .context("pending frozen historical IR 未绑定 task card")?
        .clone();

    let lineage = archived_attempt_and_collect(
        events,
        task_id,
        round,
        &payload.attempt_id,
        &payload.head_sha,
    )?;
    validate_root_authorization_order(
        root,
        events,
        round,
        task_id,
        payload.ir_revision,
        &payload.validation_digest,
        &payload.attempt_id,
        payload.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        *root_position,
    )?;
    if lineage.attempt_no != payload.attempt_no
        || lineage.implementer_agent != payload.implementer_agent
        || lineage.collect.event_id != payload.collect_completed_event_id
    {
        bail!("pending frozen root PASS 未绑定 dispatch/collect lineage");
    }

    let mut substitutions = Vec::new();
    for review in &payload.reviews {
        let kind = if review.role == "nongate" {
            "NongateReviewDelivered"
        } else {
            "ReviewDelivered"
        };
        let delivery = review_delivery_event(
            events,
            kind,
            round,
            task_id,
            &payload.attempt_id,
            &review.role,
            &review.reviewer,
        )?
        .context("pending frozen review 缺 durable delivery")?;
        let expectation = ReviewContractExpectation::exact(
            task_id,
            round,
            &payload.attempt_id,
            &review.role,
            &review.reviewer,
            &payload.head_sha,
        )?;
        let rel = expectation.artifact_relpath();
        let committed = committed_regular_blob_bytes(
            root,
            effective_main_sha,
            &rel,
            "pending frozen committed review",
        )?;
        let artifact = check_review_artifact_contract(&committed, &expectation)?
            .context("pending frozen committed review incomplete")?;
        let body_len = artifact.substantive_body_len() as u64;
        if body_len == 0 || artifact.verdict() != "PASS" {
            bail!("pending frozen committed review 非 substantive PASS");
        }
        let (sha256, bytes) = sha_binding(&committed);
        let delivery_body_len = delivery
            .payload
            .as_ref()
            .and_then(|value| value.get("bodyLen"))
            .and_then(serde_json::Value::as_u64);
        let delivery_binding_matches = if kind == "NongateReviewDelivered" {
            nongate_event_payload_str(delivery, "path") == Some(rel.as_str())
                && nongate_event_payload_str(delivery, "sha256") == Some(sha256.as_str())
                && nongate_event_payload_str(delivery, "verdict") == Some(artifact.verdict())
                && delivery
                    .payload
                    .as_ref()
                    .and_then(|value| value.get("bytes"))
                    .and_then(serde_json::Value::as_u64)
                    == Some(bytes)
                && delivery_body_len == Some(body_len)
        } else {
            delivery_body_len == Some(body_len)
        };
        if review.path != rel
            || review.role != expectation.role()
            || review.reviewer != expectation.reviewer()
            || review.verdict != artifact.verdict()
            || review.sha256 != sha256
            || review.bytes != bytes
            || review.delivery_event_id.as_deref() != Some(delivery.event_id.as_str())
            || !delivery_binding_matches
        {
            bail!("pending frozen review binding bytes/identity 漂移");
        }
        match (
            review.substituted_role.as_deref(),
            review.substituted_agent.as_deref(),
            review.source_terminal_event_id.as_deref(),
            review.delivery_event_id.as_deref(),
        ) {
            (None, None, None, _) => {}
            (Some(role), Some(agent), Some(terminal), Some(delivery_id)) => {
                substitutions.push(ReviewSeatSubstitutionBinding {
                    role: role.to_string(),
                    from_agent: agent.to_string(),
                    to_agent: review.reviewer.clone(),
                    reviewed_head: payload.head_sha.clone(),
                    source_terminal_event_id: terminal.to_string(),
                    nongate_delivery_event_id: delivery_id.to_string(),
                });
            }
            _ => bail!("pending frozen review substitution metadata 非闭合"),
        }
    }
    let review_mode = review_contract_mode_for_attempt(
        root,
        round,
        events,
        task_id,
        &payload.attempt_id,
        historical_task,
    )?;
    if review_mode == ReviewContractMode::Quorum {
        validate_archived_review_transition_chain(
            events,
            round,
            task_id,
            *root_position,
            &payload,
            historical_task,
        )?;
        let replayed_substitutions = validate_archived_quorum_binding_membership(
            &payload.reviews,
            &payload.evidence,
            historical_task,
            &payload.head_sha,
        )?;
        if replayed_substitutions != substitutions {
            bail!("pending frozen quorum/substitution replay 与 root payload 漂移");
        }
    }
    validate_review_substitution_events(
        events,
        round,
        task_id,
        &payload.attempt_id,
        *root_position,
        &substitutions,
        Some(&root_event.event_id),
    )?;
    enforce_signed_smartclaw_fixed_primary_pass_v1(
        root,
        events,
        round,
        task_id,
        &payload.attempt_id,
        &payload.head_sha,
        historical_task,
        RootVerdict::Pass,
        &payload.reviews,
    )?;
    for evidence in &payload.evidence {
        let committed = committed_regular_blob_bytes(
            root,
            effective_main_sha,
            &evidence.path,
            "pending evidence",
        )?;
        let (sha256, len) = sha_binding(&committed);
        if sha256 != evidence.sha256 || len != evidence.bytes {
            bail!("pending frozen evidence bytes 漂移: {}", evidence.path);
        }
        let _: serde_json::Value = serde_json::from_slice(&committed)
            .with_context(|| format!("pending frozen evidence 非 JSON: {}", evidence.path))?;
    }
    validate_archived_root_gate_bindings_v1(
        root,
        events,
        round,
        task_id,
        *root_position,
        0,
        &historical_ir,
        historical_task,
        &payload,
    )?;
    let bound_artifacts = payload
        .reviews
        .iter()
        .map(|binding| binding.path.clone())
        .chain(payload.evidence.iter().map(|binding| binding.path.clone()))
        .collect::<Vec<_>>();
    validate_merge_commit_shape(
        root,
        &merged.merge_sha,
        &payload.main_head_sha,
        &payload.head_sha,
        &bound_artifacts,
    )?;

    let authorization = RootRecordAuthorization {
        verdict_event_id: root_event.event_id.clone(),
        plan_signed_off_event_id,
        attempt_id: payload.attempt_id,
        attempt_no: payload.attempt_no,
        implementer_agent: payload.implementer_agent,
        head_sha: payload.head_sha,
        expected_main_sha: payload.main_head_sha,
        merge_sha: merged.merge_sha,
        ir_revision: payload.ir_revision,
        validation_digest: payload.validation_digest,
        task_card_sha256,
        reviews: payload.reviews,
        already_recorded: false,
    };
    let card =
        frozen_contract_card_from_record_authorization(root, round, task_id, &authorization)?;
    Ok(Some((authorization, card)))
}

/// Admit a new TaskRecorded group under the existing lifecycle safety checks.
/// New supersession events are rejected even with the exclusive capability;
/// historical groups are validated separately by the read-only replay path.
pub(crate) fn validate_frozen_contract_supersession_append(
    root: &Path,
    round: &str,
    existing: &[EventRecord],
    proposed: &[EventRecord],
) -> Result<()> {
    if proposed.iter().any(|event| event.kind == "FrozenContractSuperseded") {
        bail!("FrozenContractSuperseded is historical audit data; new events are retired");
    }
    let accepted = validate_canonical_recorded_batches(root, round, existing, proposed, true)?;
    let proposed_ids = proposed
        .iter()
        .filter(|event| event.kind == "FrozenContractSuperseded")
        .map(|event| event.event_id.clone())
        .collect::<BTreeSet<_>>();
    if accepted != proposed_ids {
        bail!("merge-lifecycle FrozenContractSuperseded suffix 非完整 canonical batch");
    }
    let recorded_at = proposed
        .iter()
        .position(|event| event.kind == "TaskRecorded")
        .context("FrozenContractSuperseded batch 缺 TaskRecorded")?;
    if proposed
        .iter()
        .enumerate()
        .any(|(position, event)| event.kind == "TaskRecorded" && position != recorded_at)
    {
        bail!("FrozenContractSuperseded batch 含多条 TaskRecorded");
    }
    if recorded_at > 1 || (recorded_at == 1 && proposed[0].kind != "MergeExecuted") {
        bail!("FrozenContractSuperseded batch 在 TaskRecorded 前含非 canonical 前缀");
    }
    Ok(())
}

/// Revalidate all Effective supersessions in ledger order and return the only
/// counts permitted to enter RoundClosed.  Authorized card declarations with
/// no adjacent durable event never reach this map.
/// Schema 3 has no permanent supersessions: reject any retired event and return
/// zero counts without invoking the legacy recorded-batch auditor. Round close
/// independently validates every recorded task chain before consuming counts.
pub fn validated_frozen_contract_supersession_counts(
    root: &Path,
    round: &str,
    events: &[EventRecord],
) -> Result<BTreeMap<String, u64>> {
    if crate::round::contract_schema_from_events(events, round)?
        == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        if events
            .iter()
            .any(|event| event.kind == "FrozenContractSuperseded")
        {
            bail!("schema 3 round close 不得包含 FrozenContractSuperseded");
        }
        return Ok(BTreeMap::new());
    }
    let accepted = validate_canonical_recorded_batches(root, round, &[], events, false)?;
    let mut counts = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.kind == "FrozenContractSuperseded" && accepted.contains(&event.event_id)
    }) {
        let payload: FrozenContractSupersededPayload = serde_json::from_value(
            event
                .payload
                .clone()
                .context("count FrozenContractSuperseded 缺 payload")?,
        )?;
        *counts.entry(payload.initiator).or_insert(0) += 1;
    }
    Ok(counts)
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
        "requestedProvider",
        "requestedModel",
        "requestedEffort",
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
    if (payload.len() != KEYS.len() && payload.len() != KEYS.len() + 1)
        || KEYS.iter().any(|key| !payload.contains_key(*key))
    {
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
                && matches!(*role, "primary" | "secondary" | "nongate" | "review")
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
    if !matches!(
        provider,
        "codex" | "opencode" | "smartclaw" | "pi" | "zcode" | "agy" | "dsh"
    ) || required("receiptKind")? != provider
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
    let observed_session_valid = if matches!(provider, "zcode" | "agy") {
        payload
            .get("observedSessionId")
            .is_some_and(serde_json::Value::is_null)
    } else {
        payload
            .get("observedSessionId")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
    };
    if !observed_session_valid {
        bail!("wake-backend-receipt observedSessionId is invalid");
    }
    let signed_pin = matches!(provider, "pi" | "zcode" | "agy" | "dsh");
    for key in ["requestedProvider", "requestedModel", "requestedEffort"] {
        let valid = if signed_pin {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| !value.is_empty())
        } else {
            payload.get(key).is_some_and(serde_json::Value::is_null)
        };
        if !valid {
            bail!("wake-backend-receipt signed pin shape is invalid: {key}");
        }
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
    // The producer appends exactly one additional field for a unified action.
    // Accepting that field recognizes an existing receipt; it proves no terminal.
    let binding = crate::wake::channel_action_binding_from_wake(wake)?;
    if payload.len() != KEYS.len() + usize::from(binding.is_some()) {
        bail!("wake-backend-receipt payload shape disagrees with issued channel binding");
    }
    crate::wake::validate_existing_channel_action_binding(wake, event)?;
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
            == receipt_request_session
        && (!signed_pin
            || ["requestedProvider", "requestedModel", "requestedEffort"]
                .iter()
                .all(|key| {
                    wake.payload.as_ref().and_then(|value| value.get(*key)) == payload.get(*key)
                }));
    if !same {
        bail!("wake-backend-receipt does not exactly bind its preceding WakeIssued");
    }
    if review_continuation {
        if event_payload_str(wake, "panelId").is_some() {
            let routes = prior
                .iter()
                .filter(|candidate| {
                    candidate.kind == "ReviewSeatRouted"
                        && candidate.actor == "runtime:orch"
                        && candidate.task_id.as_deref() == Some(task_id)
                        && candidate.round.as_deref() == Some(round)
                        && event_payload_str(candidate, "wakeId") == Some(wake_id)
                        && event_payload_str(candidate, "attemptId") == Some(attempt_id)
                        && event_payload_str(candidate, "agent") == Some(agent)
                        && candidate.event_id
                            == event_payload_str(wake, "routeEventId").unwrap_or_default()
                        && event_payload_str(candidate, "panelId")
                            == event_payload_str(wake, "panelId")
                        && event_payload_str(candidate, "seatId")
                            == event_payload_str(wake, "seatId")
                        && candidate
                            .payload
                            .as_ref()
                            .and_then(|value| value.get("generation"))
                            == wake
                                .payload
                                .as_ref()
                                .and_then(|value| value.get("generation"))
                        && event_payload_str(candidate, "policyBaseSha")
                            == event_payload_str(wake, "policyBaseSha")
                        && event_payload_str(candidate, "role")
                            == continuation_parts.get(4).copied()
                })
                .count();
            if routes != 1 {
                bail!("panel wake-backend-receipt requires one preceding ReviewSeatRouted");
            }
        }
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
        let role = continuation_parts.get(4).copied().unwrap_or_default();
        if role != "nongate" && reviews != 1 {
            bail!("review wake-backend-receipt requires one preceding ReviewRequested");
        }
    }
    Ok(true)
}

fn canonical_review_quorum_suffix_event(event: &EventRecord, round: &str) -> Result<bool> {
    if !matches!(
        event.kind.as_str(),
        "ReviewFallbackSelected" | "NongateReviewDelivered" | "ReviewSeatSubstituted"
    ) {
        return Ok(false);
    }
    if event.actor != "runtime:orch"
        || event.round.as_deref() != Some(round)
        || event.task_id.as_deref().is_none_or(str::is_empty)
    {
        bail!("review quorum suffix event envelope 非 canonical");
    }
    let payload = event
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("review quorum suffix event payload 缺失")?;
    let required_string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("{} payload.{key} 缺失", event.kind))
    };
    required_string("attemptId")?;
    let reviewed_head = required_string("reviewedHead")?;
    full_sha(reviewed_head, "review quorum suffix reviewedHead")?;
    match event.kind.as_str() {
        "ReviewFallbackSelected" => {
            for key in [
                "role",
                "fromAgent",
                "toAgent",
                "sourceWakeId",
                "terminalEventId",
                "targetWakeId",
            ] {
                required_string(key)?;
            }
        }
        "NongateReviewDelivered" => {
            if required_string("role")? != "nongate" {
                bail!("NongateReviewDelivered role 必须为 nongate");
            }
            for key in [
                "agent",
                "path",
                "sha256",
                "verdict",
                "receiptState",
                "wakeId",
                "wakeIssuedEventId",
                "workspaceLeasedEventId",
            ] {
                required_string(key)?;
            }
            full_sha256(required_string("sha256")?, "NongateReviewDelivered.sha256")?;
            if !matches!(required_string("verdict")?, "PASS" | "FAIL" | "BLOCKED")
                || !matches!(
                    required_string("receiptState")?,
                    "answered" | "failed" | "timedOut" | "empty"
                )
                || payload
                    .get("bytes")
                    .and_then(serde_json::Value::as_u64)
                    .is_none_or(|value| value == 0)
                || payload
                    .get("bodyLen")
                    .and_then(serde_json::Value::as_u64)
                    .is_none_or(|value| value == 0)
            {
                bail!("NongateReviewDelivered verdict/receipt/byte counts 非 canonical");
            }
        }
        "ReviewSeatSubstituted" => {
            for key in [
                "role",
                "fromAgent",
                "toAgent",
                "sourceTerminalEventId",
                "nongateDeliveryEventId",
                "verdictEventId",
            ] {
                required_string(key)?;
            }
        }
        _ => unreachable!(),
    }
    Ok(true)
}

fn complete_seal_terminal_start_for_mode(
    prior: &[EventRecord],
    suffix: &[EventRecord],
    round: &str,
    task_id: &str,
    ledger_mode: CommittedLedgerMode,
) -> Result<Option<usize>> {
    if !matches!(ledger_mode, CommittedLedgerMode::CanonicalPostMergeSuffix) {
        return Ok(None);
    }
    let candidate_start = match suffix.last() {
        Some(event) if event.kind == "ManagedWakeTerminated" => Some(suffix.len() - 1),
        Some(event)
            if event.kind == "WorkspaceReleased"
                && suffix.len() >= 2
                && suffix[suffix.len() - 2].kind == "ManagedWakeTerminated" =>
        {
            Some(suffix.len() - 2)
        }
        _ => None,
    };
    let Some(start) = candidate_start else {
        return Ok(None);
    };
    let mut visible = prior.to_vec();
    visible.extend_from_slice(&suffix[..start]);
    Ok(canonical_complete_seal_managed_terminal_suffix_v1(
        &visible,
        &suffix[start..],
        round,
        task_id,
    )?
    .then_some(start))
}

#[cfg(test)]
mod complete_seal_terminal_mode_tests {
    use super::*;

    #[test]
    fn managed_terminal_permission_is_postmerge_only() {
        let round = "r82";
        let task = "B313";
        let wake_id = "01a0b313-1111-4222-8333-444455556666";
        let agent = "executor-desktop";
        let prior = vec![
            ledger::event(
                "WakeIssued",
                "runtime:orch",
                Some(task),
                Some(round),
                serde_json::json!({"wakeId": wake_id, "agent": agent}),
            ),
            ledger::event(
                "TaskRecorded",
                "runtime:orch",
                Some(task),
                Some(round),
                serde_json::json!({"postMergeGates": "all-green"}),
            ),
        ];
        let suffix = vec![ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(task),
            Some(round),
            serde_json::json!({
                "wakeId": wake_id,
                "agent": agent,
                "completionReason": "natural-exit",
                "terminalSeen": false,
                "exitedNaturally": true,
                "hardDeadlineReached": false,
                "cancelRequestId": null,
                "signals": [],
                "managedScopeTerminated": true,
                "logBytesRead": 41,
                "outcomeClass": "TruncatedNoTerminal",
            }),
        )];

        for mode in [
            CommittedLedgerMode::Exact,
            CommittedLedgerMode::CanonicalStorageSuffix,
            CommittedLedgerMode::CanonicalVerdictSuffix,
            CommittedLedgerMode::CanonicalRootSuffix,
        ] {
            assert_eq!(
                complete_seal_terminal_start_for_mode(&prior, &suffix, round, task, mode).unwrap(),
                None
            );
        }
        assert_eq!(
            complete_seal_terminal_start_for_mode(
                &prior,
                &suffix,
                round,
                task,
                CommittedLedgerMode::CanonicalPostMergeSuffix,
            )
            .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn completed_replay_accepts_multiple_cleanup_only_terminal_batches() {
        let round = "r83";
        let task = "B319";
        let agents = ["executor-claw", "executor-opencode"];
        let wakes = [
            "01a0b319-1111-4222-8333-444455556666",
            "01a0b319-7777-4888-8999-aaaabbbbcccc",
        ];
        let mut prior = agents
            .iter()
            .zip(wakes)
            .map(|(agent, wake_id)| {
                ledger::event(
                    "WakeIssued",
                    "runtime:orch",
                    Some(task),
                    Some(round),
                    serde_json::json!({"wakeId": wake_id, "agent": agent}),
                )
            })
            .collect::<Vec<_>>();
        prior.push(ledger::event(
            "TaskRecorded",
            "runtime:orch",
            Some(task),
            Some(round),
            serde_json::json!({"postMergeGates": "all-green"}),
        ));
        let suffix = agents
            .iter()
            .zip(wakes)
            .map(|(agent, wake_id)| {
                ledger::event(
                    "ManagedWakeTerminated",
                    "runtime:orch",
                    Some(task),
                    Some(round),
                    serde_json::json!({
                        "wakeId": wake_id, "agent": agent,
                        "completionReason": "natural-exit",
                        "terminalSeen": false, "exitedNaturally": true,
                        "hardDeadlineReached": false, "cancelRequestId": null,
                        "signals": [], "managedScopeTerminated": true,
                        "logBytesRead": 1, "outcomeClass": "TruncatedNoTerminal"
                    }),
                )
            })
            .collect::<Vec<_>>();
        assert!(!canonical_complete_seal_managed_terminal_suffix_v1(
            &prior, &suffix, round, task
        )
        .unwrap());
        assert!(canonical_complete_seal_managed_terminal_suffixes_v1(
            &prior, &suffix, round, task
        )
        .unwrap());
    }
}

fn validate_expected_main_contract(
    root: &Path,
    round: &str,
    task_id: &str,
    main_sha: &str,
    active: &crate::plan::ReadonlyIrValidation,
    ledger_mode: CommittedLedgerMode,
) -> Result<()> {
    let schema3 = active.candidate.schema_version
        == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION;
    let mut sources = vec![(
        "coordination/PROJECT-BINDING.yaml".to_string(),
        Some(active.candidate.source_bindings.binding_sha256.as_str()),
    )];
    if !schema3 {
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
        sources.push((
            mode_rel,
            Some(active.candidate.source_bindings.mode_sha256.as_str()),
        ));
    }
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
            let mut runtime_history = prior.clone();
            runtime_history.extend(suffix_events.iter().cloned());
            crate::ledger::validate_runtime_event_history_v1_at_root(
                root,
                &runtime_history,
                round,
            )?;
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
            let frozen_supersessions =
                if matches!(ledger_mode, CommittedLedgerMode::CanonicalPostMergeSuffix) {
                    validate_canonical_recorded_batches(root, round, &prior, &suffix_events, false)?
                } else {
                    BTreeSet::new()
                };
            let complete_seal_terminal_start = complete_seal_terminal_start_for_mode(
                &prior,
                &suffix_events,
                round,
                task_id,
                ledger_mode,
            )?;
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
            let mut complete_quarantine_history = prior.clone();
            complete_quarantine_history.extend(suffix_events.iter().cloned());
            for (index, event) in suffix_events.into_iter().enumerate() {
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
                let review_quarantine = crate::generic_review::canonical_review_quarantine(
                    &event, &complete_quarantine_history, round,
                )?;
                let late_review = late.event_ids.contains(&event.event_id);
                let frozen_contract_supersession = frozen_supersessions.contains(&event.event_id);
                let budget_threshold = canonical_budget_threshold_suffix(&event, round);
                // Storage admission is an inert same-round fact.  It is intentionally
                // task-agnostic here so a concurrent task cannot make an otherwise valid
                // expected-main snapshot stale; Active-barrier append authority remains
                // exact-task in ledger::validate_storage_audit_append.
                let storage = crate::ledger::canonical_gate_storage_audit_event(&event)
                    .is_some_and(|audit| audit.round == round);
                let gate_executed = canonical_gate_executed_payload(&event, round).is_some();
                let review_quorum_event = canonical_review_quorum_suffix_event(&event, round)?;
                let runtime_v1 =
                    runtime_event_allowed_in_committed_mode(&event, round, ledger_mode)?;
                let complete_seal_terminal_batch =
                    complete_seal_terminal_start.is_some_and(|start| index >= start);
                let allowed = storage
                    || gate_executed
                    || review_quorum_event
                    || runtime_v1
                    || (review_quarantine
                        && !matches!(ledger_mode, CommittedLedgerMode::CanonicalStorageSuffix))
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
                            || frozen_contract_supersession
                            || complete_seal_terminal_batch
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

fn review_delivery_event<'a>(
    events: &'a [EventRecord],
    kind: &str,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    role: &str,
    agent: &str,
) -> Result<Option<&'a EventRecord>> {
    let matching = events
        .iter()
        .filter(|event| {
            event.kind == kind
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
                && nongate_event_payload_str(event, "attemptId") == Some(attempt_id)
                && nongate_event_payload_str(event, "role") == Some(role)
                && nongate_event_payload_str(event, "agent") == Some(agent)
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [] => Ok(None),
        [event] => Ok(Some(*event)),
        _ => bail!(
            "review delivery fact 重复: kind={kind} task={task_id} attempt={attempt_id} role={role} agent={agent}"
        ),
    }
}

fn current_formal_request<'a>(
    events: &'a [EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    role: &str,
    agent: &str,
    head_sha: &str,
) -> Result<Option<&'a EventRecord>> {
    let requests = events
        .iter()
        .filter(|event| {
            event.kind == "ReviewRequested"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
                && nongate_event_payload_str(event, "attemptId") == Some(attempt_id)
                && nongate_event_payload_str(event, "role") == Some(role)
                && nongate_event_payload_str(event, "agent") == Some(agent)
        })
        .collect::<Vec<_>>();
    let Some(request) = requests.last().copied() else {
        return Ok(None);
    };
    if nongate_event_payload_str(request, "reviewedHead") != Some(head_sha) {
        bail!("formal ReviewRequested reviewedHead 漂移");
    }
    let wake_id =
        nongate_event_payload_str(request, "wakeId").context("formal ReviewRequested 缺 wakeId")?;
    let request_position = events
        .iter()
        .position(|event| event.event_id == request.event_id)
        .context("formal ReviewRequested eventId 不在账本")?;
    let wakes = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "WakeIssued"
                && nongate_event_payload_str(event, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    let [(wake_position, wake)] = wakes.as_slice() else {
        bail!("formal ReviewRequested 缺唯一、先行的 exact WakeIssued");
    };
    if wake.actor != "runtime:orch"
        || wake.round.as_deref() != Some(round)
        || wake.task_id.as_deref() != Some(task_id)
        || nongate_event_payload_str(wake, "agent") != Some(agent)
        || nongate_event_payload_str(wake, "attemptId") != Some(attempt_id)
        || *wake_position >= request_position
    {
        bail!("formal WakeIssued 必须先于 ReviewRequested");
    }
    Ok(Some(request))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedChannelTerminalDisposition {
    FallbackEligible,
    SeatClosedOnly,
}

fn managed_channel_terminal<'a>(
    events: &'a [EventRecord],
    round: &str,
    task_id: &str,
    agent: &str,
    wake_id: &str,
) -> Result<Option<(&'a EventRecord, ManagedChannelTerminalDisposition)>> {
    let terminals = events
        .iter()
        .filter(|event| {
            event.kind == "ManagedWakeTerminated"
                && nongate_event_payload_str(event, "wakeId") == Some(wake_id)
        })
        .collect::<Vec<_>>();
    let terminal = match terminals.as_slice() {
        [] => return Ok(None),
        [terminal] => *terminal,
        _ => bail!("formal wake 含重复 ManagedWakeTerminated: wakeId={wake_id}"),
    };
    if terminal.actor != "runtime:orch"
        || terminal.round.as_deref() != Some(round)
        || terminal.task_id.as_deref() != Some(task_id)
        || nongate_event_payload_str(terminal, "agent") != Some(agent)
    {
        bail!("formal ManagedWakeTerminated envelope 与 exact request 漂移");
    }
    let payload = terminal
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("ManagedWakeTerminated payload 缺失")?;
    if payload
        .get("managedScopeTerminated")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Ok(None);
    }
    let outcome = payload
        .get("outcomeClass")
        .and_then(serde_json::Value::as_str)
        .context("ManagedWakeTerminated.outcomeClass 缺失")?;
    let disposition = match outcome {
        "DeliveredTerminal" | "TruncatedNoTerminal" | "StoppedByHardDeadline" => {
            ManagedChannelTerminalDisposition::FallbackEligible
        }
        "OperationalError" => {
            let Some(facts) = managed_terminal_facts(terminal, round) else {
                return Ok(None);
            };
            if facts.wake_id != wake_id || facts.agent != agent || !facts.managed_scope_terminated {
                return Ok(None);
            }
            ManagedChannelTerminalDisposition::SeatClosedOnly
        }
        "StoppedByAuthenticatedCancel" => return Ok(None),
        _ => return Ok(None),
    };
    Ok(Some((terminal, disposition)))
}

fn managed_channel_terminal_after_request<'a>(
    events: &'a [EventRecord],
    round: &str,
    task_id: &str,
    agent: &str,
    request: &EventRecord,
) -> Result<Option<(&'a EventRecord, ManagedChannelTerminalDisposition)>> {
    let wake_id =
        nongate_event_payload_str(request, "wakeId").context("formal ReviewRequested 缺 wakeId")?;
    let Some((terminal, disposition)) =
        managed_channel_terminal(events, round, task_id, agent, wake_id)?
    else {
        return Ok(None);
    };
    let request_position = events
        .iter()
        .position(|event| event.event_id == request.event_id)
        .context("formal ReviewRequested 不在账本")?;
    let terminal_position = events
        .iter()
        .position(|event| event.event_id == terminal.event_id)
        .context("formal ManagedWakeTerminated 不在账本")?;
    if request_position >= terminal_position {
        bail!("formal ManagedWakeTerminated 必须晚于 exact ReviewRequested");
    }
    Ok(Some((terminal, disposition)))
}

fn checked_review_binding(
    root: &Path,
    main_sha: &str,
    expectation: &ReviewContractExpectation,
    delivery: &EventRecord,
) -> Result<(ReviewBinding, ReviewResultState, u64)> {
    let rel = expectation.artifact_relpath();
    let path = root.join(&rel);
    let bytes = regular_file_bytes(root, &path, "quorum review")?;
    let committed = committed_regular_blob_bytes(root, main_sha, &rel, "quorum review")?;
    if bytes != committed {
        bail!("quorum review 当前 bytes 与 expected main blob 不一致: {rel}");
    }
    let checked = check_review_artifact_contract(&bytes, expectation)
        .with_context(|| format!("quorum review contract 失败: {rel}"))?
        .with_context(|| format!("quorum review 尚未完成: {rel}"))?;
    if checked.substantive_body_len() == 0 {
        bail!("quorum review 缺 substantive body: {rel}");
    }
    report_ignored_review_fields(&rel, checked.ignored_fields());
    let state = match (expectation.role(), checked.verdict()) {
        ("nongate", "PASS") => ReviewResultState::NongatePass,
        ("nongate", "FAIL") => ReviewResultState::NongateFail,
        ("nongate", "BLOCKED") => ReviewResultState::Blocked,
        ("primary" | "secondary", "PASS") => ReviewResultState::FormalPass,
        ("primary" | "secondary", "FAIL") => ReviewResultState::FormalFail,
        ("primary" | "secondary", "BLOCKED") => ReviewResultState::Blocked,
        _ => bail!("quorum review role/verdict 组合未建模"),
    };
    let (sha256, len) = sha_binding(&committed);
    let body_len = checked.substantive_body_len() as u64;
    Ok((
        ReviewBinding {
            path: rel,
            role: expectation.role().to_string(),
            reviewer: expectation.reviewer().to_string(),
            verdict: checked.verdict().to_string(),
            sha256,
            bytes: len,
            delivery_event_id: Some(delivery.event_id.clone()),
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        },
        state,
        body_len,
    ))
}

fn superseded_source_finding(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    role: &str,
    agent: &str,
    head_sha: &str,
) -> Result<Option<String>> {
    let expectation =
        ReviewContractExpectation::exact(task_id, round, attempt_id, role, agent, head_sha)?;
    for (rel, label) in [
        (
            expectation.artifact_relpath(),
            "verdict-bound canonical review",
        ),
        (
            wake::review_inbox_relpath(round, attempt_id, role, agent)?,
            "staged review inbox artifact",
        ),
    ] {
        let Some(bytes) = optional_review_artifact_bytes(root, &root.join(&rel), label)? else {
            continue;
        };
        let Some(checked) = check_review_artifact_contract(&bytes, &expectation)? else {
            bail!("superseded source artifact is partial: {rel}");
        };
        if checked.substantive_body_len() == 0 {
            bail!("superseded source artifact lacks substantive body: {rel}");
        }
        if matches!(checked.verdict(), "FAIL" | "BLOCKED") {
            return Ok(Some(format!(
                "superseded formal reviewer {agent} delivered monotonic {} finding at {rel}",
                checked.verdict()
            )));
        }
    }
    Ok(None)
}

fn quorum_file_bindings(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    head_sha: &str,
    main_sha: &str,
    implementer_agent: &str,
    ir_task: &crate::plan::IrTask,
    verdict: RootVerdict,
) -> Result<(
    Vec<ReviewBinding>,
    Vec<EvidenceBinding>,
    Vec<ReviewSeatSubstitutionBinding>,
)> {
    let policy = ir_task
        .review_quorum
        .as_ref()
        .context("quorum binding called without signed reviewQuorum")?;
    let mut results = Vec::new();
    let mut reviews = Vec::new();
    let mut formal_errors = Vec::<(String, String, String)>::new();

    for required in &ir_task.required_reviews {
        let signed_fallback = signed_review_fallback(ir_task, &required.role);
        if required.agent == implementer_agent
            || signed_fallback.is_some_and(|agent| agent == implementer_agent)
        {
            bail!("formal reviewer/fallback 与 current implementer 重叠");
        }
        let selections = events
            .iter()
            .enumerate()
            .filter(|event| {
                let event = event.1;
                event.kind == "ReviewFallbackSelected"
                    && event.round.as_deref() == Some(round)
                    && event.task_id.as_deref() == Some(task_id)
                    && nongate_event_payload_str(event, "attemptId") == Some(attempt_id)
                    && nongate_event_payload_str(event, "role") == Some(required.role.as_str())
            })
            .collect::<Vec<_>>();
        if selections.len() > 1 {
            bail!("formal seat 含重复 ReviewFallbackSelected");
        }
        let mut reviewer = required.agent.as_str();
        let mut fallback_selected = false;
        let mut selected_event = None;
        if let Some((selection_position, selection)) = selections.first().copied() {
            let target = signed_fallback.context("unsigned ReviewFallbackSelected")?;
            if selection.actor != "runtime:orch"
                || nongate_event_payload_str(selection, "fromAgent")
                    != Some(required.agent.as_str())
                || nongate_event_payload_str(selection, "toAgent") != Some(target)
                || nongate_event_payload_str(selection, "reviewedHead") != Some(head_sha)
            {
                bail!("ReviewFallbackSelected 未绑定 signed exact tuple");
            }
            let terminal_id = nongate_event_payload_str(selection, "terminalEventId")
                .context("ReviewFallbackSelected 缺 terminalEventId")?;
            let terminal = events
                .iter()
                .enumerate()
                .filter(|(_, event)| event.event_id == terminal_id)
                .collect::<Vec<_>>();
            let [(terminal_position, terminal)] = terminal.as_slice() else {
                bail!("ReviewFallbackSelected terminalEventId 不唯一");
            };
            let source_wake_id = nongate_event_payload_str(selection, "sourceWakeId")
                .context("ReviewFallbackSelected 缺 sourceWakeId")?;
            let source_request = current_formal_request(
                events,
                round,
                task_id,
                attempt_id,
                &required.role,
                &required.agent,
                head_sha,
            )?
            .context("ReviewFallbackSelected source 缺 exact ReviewRequested")?;
            if nongate_event_payload_str(source_request, "wakeId") != Some(source_wake_id)
                || managed_channel_terminal_after_request(
                    events,
                    round,
                    task_id,
                    &required.agent,
                    source_request,
                )?
                .is_none_or(|(event, disposition)| {
                    disposition != ManagedChannelTerminalDisposition::FallbackEligible
                        || event.event_id != terminal.event_id
                })
            {
                bail!("ReviewFallbackSelected terminal chain 不合法");
            }
            if *terminal_position >= selection_position {
                bail!("ReviewFallbackSelected 必须晚于 source terminal fact");
            }
            if let Some(finding) = superseded_source_finding(
                root,
                round,
                task_id,
                attempt_id,
                &required.role,
                &required.agent,
                head_sha,
            )? {
                if verdict == RootVerdict::Pass {
                    bail!("{finding}");
                }
            }
            reviewer = target;
            fallback_selected = true;
            selected_event = Some((selection_position, selection));
        }

        let expectation = ReviewContractExpectation::exact(
            task_id,
            round,
            attempt_id,
            &required.role,
            reviewer,
            head_sha,
        )?;
        let delivery = review_delivery_event(
            events,
            "ReviewDelivered",
            round,
            task_id,
            attempt_id,
            &required.role,
            reviewer,
        )?;
        let request = current_formal_request(
            events,
            round,
            task_id,
            attempt_id,
            &required.role,
            reviewer,
            head_sha,
        )?;
        if root.join(expectation.artifact_relpath()).exists() {
            let delivery = delivery.context("formal artifact 已在但缺 durable ReviewDelivered")?;
            let request = request.context("formal ReviewDelivered 缺 exact ReviewRequested")?;
            let request_position = events
                .iter()
                .position(|event| event.event_id == request.event_id)
                .context("formal ReviewRequested 不在账本")?;
            let delivery_position = events
                .iter()
                .position(|event| event.event_id == delivery.event_id)
                .context("formal ReviewDelivered 不在账本")?;
            if request_position >= delivery_position {
                bail!("formal ReviewDelivered 必须晚于 exact ReviewRequested");
            }
            let (binding, state, body_len) =
                checked_review_binding(root, main_sha, &expectation, delivery)?;
            if delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bodyLen"))
                .and_then(serde_json::Value::as_u64)
                != Some(body_len)
            {
                bail!("formal ReviewDelivered.bodyLen 与 bound artifact 不一致");
            }
            results.push(ReviewResult {
                agent: reviewer.to_string(),
                role: required.role.clone(),
                attempt_id: attempt_id.to_string(),
                reviewed_head: head_sha.to_string(),
                delivery_event_id: delivery.event_id.clone(),
                state,
            });
            reviews.push(binding);
            continue;
        }
        if delivery.is_some() {
            bail!("formal ReviewDelivered 缺对应 canonical artifact");
        }
        let Some(request) = request else {
            results.push(ReviewResult {
                agent: reviewer.to_string(),
                role: required.role.clone(),
                attempt_id: attempt_id.to_string(),
                reviewed_head: head_sha.to_string(),
                delivery_event_id: format!("pending:{}:{}", required.role, reviewer),
                state: ReviewResultState::Pending,
            });
            continue;
        };
        if let Some((selection_position, selection)) = selected_event {
            let target_wake_id = nongate_event_payload_str(selection, "targetWakeId")
                .context("ReviewFallbackSelected 缺 targetWakeId")?;
            if nongate_event_payload_str(request, "reviewFallbackEventId")
                != Some(selection.event_id.as_str())
                || nongate_event_payload_str(request, "wakeId") != Some(target_wake_id)
            {
                bail!("fallback target ReviewRequested 未反向绑定 selection/wake");
            }
            let request_position = events
                .iter()
                .position(|event| event.event_id == request.event_id)
                .context("fallback target ReviewRequested 不在账本")?;
            let target_wakes = events
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    event.kind == "WakeIssued"
                        && event.actor == "runtime:orch"
                        && event.round.as_deref() == Some(round)
                        && event.task_id.as_deref() == Some(task_id)
                        && nongate_event_payload_str(event, "wakeId") == Some(target_wake_id)
                        && nongate_event_payload_str(event, "agent") == Some(reviewer)
                        && nongate_event_payload_str(event, "reviewFallbackEventId")
                            == Some(selection.event_id.as_str())
                })
                .collect::<Vec<_>>();
            let [(wake_position, _)] = target_wakes.as_slice() else {
                bail!("fallback selection 缺唯一 target WakeIssued");
            };
            if selection_position >= *wake_position || *wake_position >= request_position {
                bail!("fallback durable order 不是 selected → wake → requested");
            }
        }
        let terminal =
            managed_channel_terminal_after_request(events, round, task_id, reviewer, request)?;
        let state = match terminal {
            Some((_, ManagedChannelTerminalDisposition::SeatClosedOnly)) => {
                ReviewResultState::OperationalError
            }
            Some((_, ManagedChannelTerminalDisposition::FallbackEligible))
                if fallback_selected || signed_fallback.is_none() =>
            {
                ReviewResultState::ChannelError
            }
            _ => ReviewResultState::Pending,
        };
        let event_id = terminal
            .map(|(event, _)| event.event_id.clone())
            .unwrap_or_else(|| request.event_id.clone());
        if state == ReviewResultState::ChannelError {
            formal_errors.push((
                required.role.clone(),
                reviewer.to_string(),
                event_id.clone(),
            ));
        }
        results.push(ReviewResult {
            agent: reviewer.to_string(),
            role: required.role.clone(),
            attempt_id: attempt_id.to_string(),
            reviewed_head: head_sha.to_string(),
            delivery_event_id: event_id,
            state,
        });
    }

    let mut nongate_pass_bindings = Vec::<(usize, String, String)>::new();
    for seat in &ir_task.nongate_seats {
        if seat.agent == implementer_agent {
            bail!("nongate reviewer 与 current implementer 重叠");
        }
        let receipt = match validate_nongate_attempt_receipt_binding(
            root,
            attempt_id,
            round,
            task_id,
            head_sha,
            &seat.agent,
            events,
        ) {
            Ok(receipt) => Some(receipt),
            Err(_) if verdict != RootVerdict::Pass => None,
            Err(error) => bail!(
                "signed nongate obligation 缺 exact attempt receipt: agent={} ({error})",
                seat.agent
            ),
        };
        let Some(receipt) = receipt else {
            continue;
        };
        if receipt.preset != seat.preset {
            bail!("nongate receipt preset 与 signed seat 不一致");
        }
        let Some(delivery) = review_delivery_event(
            events,
            "NongateReviewDelivered",
            round,
            task_id,
            attempt_id,
            "nongate",
            &seat.agent,
        )?
        else {
            continue;
        };
        if nongate_event_payload_str(delivery, "reviewedHead") != Some(head_sha) {
            bail!("NongateReviewDelivered reviewedHead 漂移");
        }
        let expectation = ReviewContractExpectation::exact(
            task_id,
            round,
            attempt_id,
            "nongate",
            &seat.agent,
            head_sha,
        )?;
        let (binding, state, body_len) =
            checked_review_binding(root, main_sha, &expectation, delivery)?;
        if nongate_event_payload_str(delivery, "path") != Some(binding.path.as_str())
            || nongate_event_payload_str(delivery, "sha256") != Some(binding.sha256.as_str())
            || delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bytes"))
                .and_then(serde_json::Value::as_u64)
                != Some(binding.bytes)
            || delivery
                .payload
                .as_ref()
                .and_then(|payload| payload.get("bodyLen"))
                .and_then(serde_json::Value::as_u64)
                != Some(body_len)
            || nongate_event_payload_str(delivery, "verdict") != Some(binding.verdict.as_str())
            || nongate_event_payload_str(delivery, "receiptState") != Some(receipt.state.as_str())
            || nongate_event_payload_str(delivery, "wakeId") != Some(receipt.wake_id.as_str())
            || nongate_event_payload_str(delivery, "wakeIssuedEventId")
                != Some(receipt.wake_issued_event_id.as_str())
            || nongate_event_payload_str(delivery, "workspaceLeasedEventId")
                != Some(receipt.workspace_leased_event_id.as_str())
        {
            bail!("NongateReviewDelivered payload 未逐字绑定 artifact + receipt");
        }
        if state == ReviewResultState::NongatePass && receipt.state != "answered" {
            bail!("positive nongate voice 没有 answered receipt");
        }
        let index = reviews.len();
        if state == ReviewResultState::NongatePass {
            nongate_pass_bindings.push((index, seat.agent.clone(), delivery.event_id.clone()));
        }
        results.push(ReviewResult {
            agent: seat.agent.clone(),
            role: "nongate".to_string(),
            attempt_id: attempt_id.to_string(),
            reviewed_head: head_sha.to_string(),
            delivery_event_id: delivery.event_id.clone(),
            state,
        });
        reviews.push(binding);
    }

    let (decision, primary_pass_alone) =
        evaluate_review_quorum_internal(policy, Some(ir_task), &results);
    if verdict == RootVerdict::Pass && decision != ReviewQuorumDecision::Satisfied {
        bail!("root PASS review quorum 未满足: {decision:?}");
    }
    if decision == ReviewQuorumDecision::Invalid {
        bail!("review quorum identity invalid");
    }

    let mut substitutions = Vec::new();
    if verdict == RootVerdict::Pass {
        if !primary_pass_alone && formal_errors.len() > nongate_pass_bindings.len() {
            bail!("formal channel errors 超过 unique substantive nongate voices");
        }
        for ((role, from_agent, terminal_event_id), (binding_index, to_agent, delivery_id)) in
            formal_errors
                .into_iter()
                .zip(nongate_pass_bindings.into_iter())
        {
            let binding = reviews
                .get_mut(binding_index)
                .context("nongate substitution binding index 漂移")?;
            binding.substituted_role = Some(role.clone());
            binding.substituted_agent = Some(from_agent.clone());
            binding.source_terminal_event_id = Some(terminal_event_id.clone());
            substitutions.push(ReviewSeatSubstitutionBinding {
                role,
                from_agent,
                to_agent,
                reviewed_head: head_sha.to_string(),
                source_terminal_event_id: terminal_event_id,
                nongate_delivery_event_id: delivery_id,
            });
        }
    }

    let mut evidence = Vec::new();
    if verdict == RootVerdict::Pass {
        for evidence_id in &ir_task.required_evidence {
            let rel = format!("coordination/rounds/{round}/evidence/{task_id}-{evidence_id}.json");
            let bytes = regular_file_bytes(root, &root.join(&rel), "required evidence")?;
            let committed =
                committed_regular_blob_bytes(root, main_sha, &rel, "required evidence")?;
            if bytes != committed {
                bail!("required evidence 当前 bytes 与 expected main blob 不一致: {rel}");
            }
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
    Ok((reviews, evidence, substitutions))
}

fn validate_review_substitution_events(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    root_position: usize,
    substitutions: &[ReviewSeatSubstitutionBinding],
    verdict_event_id: Option<&str>,
) -> Result<()> {
    let matching = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "ReviewSeatSubstituted"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
                && nongate_event_payload_str(event, "attemptId") == Some(attempt_id)
        })
        .collect::<Vec<_>>();
    if matching.len() != substitutions.len() {
        bail!("ReviewSeatSubstituted 数量与 root binding 不一致");
    }
    if !matching.is_empty()
        && (matching[0].0 + matching.len() != root_position
            || matching.windows(2).any(|pair| pair[0].0 + 1 != pair[1].0))
    {
        bail!("ReviewSeatSubstituted 必须与 VerdictIssued 同批相邻");
    }
    for ((event_position, event), expected) in matching.into_iter().zip(substitutions) {
        let terminals = events
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.event_id == expected.source_terminal_event_id
                    && candidate.kind == "ManagedWakeTerminated"
            })
            .collect::<Vec<_>>();
        let nongate_deliveries = events
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.event_id == expected.nongate_delivery_event_id
                    && candidate.kind == "NongateReviewDelivered"
            })
            .collect::<Vec<_>>();
        let ([(terminal_position, terminal)], [(delivery_position, delivery)]) =
            (terminals.as_slice(), nongate_deliveries.as_slice())
        else {
            bail!("ReviewSeatSubstituted 引用的 terminal/delivery event 不唯一");
        };
        if *terminal_position >= event_position
            || *delivery_position >= event_position
            || terminal.actor != "runtime:orch"
            || terminal.round.as_deref() != Some(round)
            || terminal.task_id.as_deref() != Some(task_id)
            || nongate_event_payload_str(terminal, "agent") != Some(expected.from_agent.as_str())
            || terminal
                .payload
                .as_ref()
                .and_then(|payload| payload.get("managedScopeTerminated"))
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || delivery.actor != "runtime:orch"
            || delivery.round.as_deref() != Some(round)
            || delivery.task_id.as_deref() != Some(task_id)
            || nongate_event_payload_str(delivery, "attemptId") != Some(attempt_id)
            || nongate_event_payload_str(delivery, "role") != Some("nongate")
            || nongate_event_payload_str(delivery, "agent") != Some(expected.to_agent.as_str())
            || nongate_event_payload_str(delivery, "reviewedHead")
                != Some(expected.reviewed_head.as_str())
        {
            bail!("ReviewSeatSubstituted 引用链未绑定先行 exact terminal/delivery facts");
        }
        if event.actor != "runtime:orch"
            || nongate_event_payload_str(event, "role") != Some(expected.role.as_str())
            || nongate_event_payload_str(event, "fromAgent") != Some(expected.from_agent.as_str())
            || nongate_event_payload_str(event, "toAgent") != Some(expected.to_agent.as_str())
            || nongate_event_payload_str(event, "reviewedHead")
                != Some(expected.reviewed_head.as_str())
            || nongate_event_payload_str(event, "sourceTerminalEventId")
                != Some(expected.source_terminal_event_id.as_str())
            || nongate_event_payload_str(event, "nongateDeliveryEventId")
                != Some(expected.nongate_delivery_event_id.as_str())
            || verdict_event_id
                .is_some_and(|id| nongate_event_payload_str(event, "verdictEventId") != Some(id))
        {
            bail!("ReviewSeatSubstituted payload 未双向绑定 root quorum tuple");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn panel_file_bindings(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    head_sha: &str,
    main_sha: &str,
    implementer_agent: &str,
    ir_task: &crate::plan::IrTask,
    verdict: RootVerdict,
) -> Result<(
    Vec<ReviewBinding>,
    Vec<EvidenceBinding>,
    Vec<ReviewSeatSubstitutionBinding>,
)> {
    let selected = events
        .iter()
        .filter_map(|event| {
            let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewPanelSelected(selected))) =
                crate::ledger::decode_runtime_event_v1(event)
            else {
                return None;
            };
            (event.task_id.as_deref() == Some(task_id)
                && selected.attempt_id == attempt_id
                && selected.reviewed_head == head_sha)
                .then_some((event, selected))
        })
        .collect::<Vec<_>>();
    let [(_, selected)] = selected.as_slice() else {
        bail!("panel root binding 要求唯一 ReviewPanelSelected");
    };
    let closed = events
        .iter()
        .filter_map(|event| {
            let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewPanelClosed(closed))) =
                crate::ledger::decode_runtime_event_v1(event)
            else {
                return None;
            };
            (closed.panel_id == selected.panel_id).then_some((event, closed))
        })
        .collect::<Vec<_>>();
    let [(closed_event, closed)] = closed.as_slice() else {
        bail!("panel root binding 要求唯一 ReviewPanelClosed");
    };
    match verdict {
        RootVerdict::Pass if closed.outcome != "pass" => {
            bail!("root PASS 要求 ReviewPanelClosed(pass)")
        }
        RootVerdict::Fail | RootVerdict::Blocked if closed.outcome == "pass" => {
            bail!("root non-PASS 不得消费 ReviewPanelClosed(pass)")
        }
        _ => {}
    }
    if closed_event.actor != "runtime:orch"
        || closed_event.round.as_deref() != Some(round)
        || closed.policy_base_sha != selected.policy_base_sha
    {
        bail!("ReviewPanelClosed envelope/policy base 漂移");
    }
    let mut current_routes = BTreeMap::<String, crate::ledger::ReviewSeatRoutedPayloadV1>::new();
    for event in events {
        let Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatRouted(route)) =
            crate::ledger::decode_runtime_event_v1(event)?
        else {
            continue;
        };
        if route.panel_id == selected.panel_id
            && current_routes
                .get(&route.seat_id)
                .is_none_or(|prior| prior.generation < route.generation)
        {
            current_routes.insert(route.seat_id.clone(), route);
        }
    }
    let mut reviews = Vec::new();
    let mut primary_pass = false;
    for route in current_routes.values() {
        if route.agent == implementer_agent {
            bail!("panel reviewer 与 implementer 重叠");
        }
        let terminals = events
            .iter()
            .filter_map(|event| {
                let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatTerminated(terminal))) =
                    crate::ledger::decode_runtime_event_v1(event)
                else {
                    return None;
                };
                (terminal.panel_id == route.panel_id
                    && terminal.seat_id == route.seat_id
                    && terminal.generation == route.generation)
                    .then_some((event, terminal))
            })
            .collect::<Vec<_>>();
        let [(_, terminal)] = terminals.as_slice() else {
            bail!("panel root binding current route 缺唯一 terminal");
        };
        if !matches!(terminal.state.as_str(), "pass" | "fail" | "blocked") {
            continue;
        }
        let delivery_id = terminal
            .delivery_event_id
            .as_deref()
            .context("substantive panel terminal 缺 deliveryEventId")?;
        let deliveries = events
            .iter()
            .filter(|event| event.event_id == delivery_id)
            .collect::<Vec<_>>();
        let [delivery] = deliveries.as_slice() else {
            bail!("panel terminal deliveryEventId 不唯一");
        };
        let expectation = ReviewContractExpectation::panel_exact(
            task_id,
            round,
            attempt_id,
            &route.role,
            &route.agent,
            head_sha,
            &route.seat_id,
            route.generation,
            &route.wake_id,
            &route.policy_base_sha,
        )?;
        let (binding, state, body_len) =
            checked_review_binding(root, main_sha, &expectation, delivery)?;
        if delivery
            .payload
            .as_ref()
            .and_then(|payload| payload.get("bodyLen"))
            .and_then(serde_json::Value::as_u64)
            != Some(body_len)
        {
            bail!("panel delivery bodyLen 与 committed artifact 漂移");
        }
        let promotions = events
            .iter()
            .filter_map(|event| {
                let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewSpoolPromoted(promotion))) =
                    crate::ledger::decode_runtime_event_v1(event)
                else {
                    return None;
                };
                (promotion.delivery_event_id == delivery_id
                    && promotion.canonical_path == binding.path
                    && promotion.sha256 == binding.sha256
                    && promotion.bytes == binding.bytes
                    && promotion.terminal_event_id == terminal.terminal_event_id)
                    .then_some(promotion)
            })
            .count();
        if promotions != 1 {
            bail!("panel delivery 未绑定唯一 ReviewSpoolPromoted bytes/terminal");
        }
        if route.lineage == "primary" && state == ReviewResultState::FormalPass {
            primary_pass = true;
        }
        reviews.push(binding);
    }
    if verdict == RootVerdict::Pass
        && (reviews.len() < 2
            || reviews.iter().any(|binding| binding.verdict != "PASS")
            || !primary_pass)
    {
        bail!("root PASS panel reviews 未满足 two-pass + primary lineage");
    }
    let mut evidence = Vec::new();
    if verdict == RootVerdict::Pass {
        for evidence_id in &ir_task.required_evidence {
            let rel = format!("coordination/rounds/{round}/evidence/{task_id}-{evidence_id}.json");
            let bytes = regular_file_bytes(root, &root.join(&rel), "required evidence")?;
            let committed =
                committed_regular_blob_bytes(root, main_sha, &rel, "required evidence")?;
            if bytes != committed {
                bail!("required evidence 当前 bytes 与 expected main blob 不一致: {rel}");
            }
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
    Ok((reviews, evidence, Vec::new()))
}

const SMARTCLAW_FIXED_PRIMARY_EVIDENCE_V1: &str =
    "smartclaw-current-attempt-primary-pass-is-canonical-and-not-replaceable-by-pool-fallback";

#[allow(clippy::too_many_arguments)]
fn enforce_signed_smartclaw_fixed_primary_pass_v1(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    head_sha: &str,
    ir_task: &crate::plan::IrTask,
    verdict: RootVerdict,
    reviews: &[ReviewBinding],
) -> Result<()> {
    if verdict != RootVerdict::Pass
        || !ir_task
            .required_evidence
            .iter()
            .any(|item| item == SMARTCLAW_FIXED_PRIMARY_EVIDENCE_V1)
    {
        return Ok(());
    }
    let signed_primary = ir_task
        .required_reviews
        .iter()
        .filter(|required| required.role == "primary")
        .collect::<Vec<_>>();
    let [signed_primary] = signed_primary.as_slice() else {
        bail!("signed SmartClaw fixed-primary gate requires one primary review declaration");
    };
    if signed_primary.agent != "executor-claw" {
        bail!("signed SmartClaw fixed-primary gate requires primary/executor-claw");
    }

    let bound_primary = reviews
        .iter()
        .filter(|binding| binding.role == "primary")
        .collect::<Vec<_>>();
    let [bound_primary] = bound_primary.as_slice() else {
        bail!("SmartClaw fixed-primary gate requires one bound primary review");
    };
    if bound_primary.reviewer != "executor-claw"
        || bound_primary.verdict != "PASS"
        || bound_primary.substituted_role.is_some()
        || bound_primary.substituted_agent.is_some()
        || bound_primary.source_terminal_event_id.is_some()
    {
        bail!("SmartClaw fixed-primary PASS binding is substituted or not canonical");
    }

    let root_positions = events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "VerdictIssued"
                && event.actor == "verifier:root"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
                && event_payload_str(event, "attemptId") == Some(attempt_id)
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    if root_positions.len() > 1 {
        bail!("SmartClaw fixed-primary gate found duplicate root verdicts");
    }
    let boundary = root_positions.first().copied().unwrap_or(events.len());
    let prefix = &events[..boundary];
    if prefix.iter().any(|event| {
        event.round.as_deref() == Some(round)
            && event.task_id.as_deref() == Some(task_id)
            && event_payload_str(event, "attemptId") == Some(attempt_id)
            && ((event.kind == "ReviewFallbackSelected"
                && event_payload_str(event, "role") == Some("primary"))
                || (event.kind == "ReviewSeatSubstituted"
                    && event_payload_str(event, "role") == Some("primary")))
    }) {
        bail!("SmartClaw fixed primary must not use fallback or seat substitution");
    }

    let panel_selected = prefix.iter().any(|event| {
        matches!(
            crate::ledger::decode_runtime_event_v1(event),
            Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewPanelSelected(ref selected)))
                if event.task_id.as_deref() == Some(task_id)
                    && selected.attempt_id == attempt_id
                    && selected.reviewed_head == head_sha
        )
    });
    let mode = if panel_selected {
        ReviewContractMode::Panel
    } else {
        review_contract_mode_for_attempt(root, round, events, task_id, attempt_id, ir_task)?
    };
    if mode == ReviewContractMode::Quorum {
        let requests = prefix
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "ReviewRequested"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round)
                    && event.task_id.as_deref() == Some(task_id)
                    && event_payload_str(event, "attemptId") == Some(attempt_id)
                    && event_payload_str(event, "role") == Some("primary")
                    && event_payload_str(event, "agent") == Some("executor-claw")
                    && event_payload_str(event, "reviewedHead") == Some(head_sha)
            })
            .collect::<Vec<_>>();
        let deliveries = prefix
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "ReviewDelivered"
                    && event.actor == "runtime:orch"
                    && event.round.as_deref() == Some(round)
                    && event.task_id.as_deref() == Some(task_id)
                    && event_payload_str(event, "attemptId") == Some(attempt_id)
                    && event_payload_str(event, "role") == Some("primary")
                    && event_payload_str(event, "agent") == Some("executor-claw")
                    && event
                        .payload
                        .as_ref()
                        .and_then(|payload| payload.get("bodyLen"))
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|len| len > 0)
            })
            .collect::<Vec<_>>();
        let ([(request_position, request)], [(delivery_position, delivery)]) =
            (requests.as_slice(), deliveries.as_slice())
        else {
            bail!("SmartClaw Quorum fixed primary requires unique request and delivery");
        };
        if request_position >= delivery_position
            || bound_primary.delivery_event_id.as_deref() != Some(delivery.event_id.as_str())
            || event_payload_str(request, "reviewFallbackEventId").is_some()
        {
            bail!("SmartClaw Quorum fixed-primary request/delivery provenance drifted");
        }
        return Ok(());
    }
    if mode != ReviewContractMode::Panel {
        bail!("signed SmartClaw fixed-primary slug requires Quorum or Panel mode");
    }

    let selected = prefix
        .iter()
        .filter_map(|event| {
            let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewPanelSelected(selected))) =
                crate::ledger::decode_runtime_event_v1(event)
            else {
                return None;
            };
            (event.task_id.as_deref() == Some(task_id)
                && selected.attempt_id == attempt_id
                && selected.reviewed_head == head_sha)
                .then_some(selected)
        })
        .collect::<Vec<_>>();
    let [selected] = selected.as_slice() else {
        bail!("SmartClaw Panel fixed primary requires one selected panel");
    };
    let mut current_routes = BTreeMap::<String, crate::ledger::ReviewSeatRoutedPayloadV1>::new();
    for event in prefix {
        let Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatRouted(route)) =
            crate::ledger::decode_runtime_event_v1(event)?
        else {
            continue;
        };
        if route.panel_id == selected.panel_id
            && current_routes
                .get(&route.seat_id)
                .is_none_or(|prior| prior.generation < route.generation)
        {
            current_routes.insert(route.seat_id.clone(), route);
        }
    }
    let routes = current_routes
        .values()
        .filter(|route| {
            route.agent == "executor-claw" && route.role == "primary" && route.lineage == "primary"
        })
        .collect::<Vec<_>>();
    let [route] = routes.as_slice() else {
        bail!("SmartClaw Panel current generation lacks unique primary lineage route");
    };
    let terminal = prefix
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "ManagedWakeTerminated"
                && event.actor == "runtime:orch"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
                && event_payload_str(event, "wakeId") == Some(route.wake_id.as_str())
                && event_payload_str(event, "agent") == Some("executor-claw")
        })
        .collect::<Vec<_>>();
    let [(terminal_position, terminal)] = terminal.as_slice() else {
        bail!("SmartClaw Panel primary lacks unique managed terminal");
    };
    let payload = terminal
        .payload
        .as_ref()
        .and_then(serde_json::Value::as_object)
        .context("SmartClaw Panel terminal payload is absent")?;
    if event_payload_str(terminal, "state") != Some("answered")
        || event_payload_str(terminal, "completionReason") != Some("natural-exit")
        || event_payload_str(terminal, "exactReason") != Some("payloads-terminal")
        || payload
            .get("terminalSeen")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || payload
            .get("turnEnded")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || payload
            .get("exitedNaturally")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || payload
            .get("hardDeadlineReached")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        || payload
            .get("managedScopeTerminated")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        || payload
            .get("processTreeTerminated")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
        || !payload
            .get("signals")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty)
        || !payload.get("usage").is_some_and(serde_json::Value::is_null)
        || event_payload_str(terminal, "usageAbsentReason").is_none_or(str::is_empty)
        || event_payload_str(terminal, "outputPath").is_none_or(str::is_empty)
        || event_payload_str(terminal, "outputSha256")
            .is_none_or(|sha| full_sha256(sha, "SmartClaw outputSha256").is_err())
        || event_payload_str(terminal, "finalTextSha256")
            .is_none_or(|sha| full_sha256(sha, "SmartClaw finalTextSha256").is_err())
    {
        bail!("SmartClaw Panel derived terminal shape is not exact");
    }
    let promotions = prefix
        .iter()
        .enumerate()
        .filter_map(|(position, event)| {
            let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewSpoolPromoted(promotion))) =
                crate::ledger::decode_runtime_event_v1(event)
            else {
                return None;
            };
            (promotion.panel_id == route.panel_id
                && promotion.seat_id == route.seat_id
                && promotion.generation == route.generation
                && promotion.wake_id == route.wake_id)
                .then_some((position, promotion))
        })
        .collect::<Vec<_>>();
    let [(promotion_position, promotion)] = promotions.as_slice() else {
        bail!("SmartClaw Panel primary lacks unique spool promotion");
    };
    let deliveries = prefix
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == promotion.delivery_event_id)
        .collect::<Vec<_>>();
    let [(delivery_position, delivery)] = deliveries.as_slice() else {
        bail!("SmartClaw Panel promotion deliveryEventId is not unique");
    };
    let seat_terminals = prefix
        .iter()
        .enumerate()
        .filter_map(|(position, event)| {
            let Ok(Some(crate::ledger::RuntimeEventPayloadV1::ReviewSeatTerminated(seat))) =
                crate::ledger::decode_runtime_event_v1(event)
            else {
                return None;
            };
            (seat.panel_id == route.panel_id
                && seat.seat_id == route.seat_id
                && seat.generation == route.generation)
                .then_some((position, seat))
        })
        .collect::<Vec<_>>();
    let [(seat_position, seat)] = seat_terminals.as_slice() else {
        bail!("SmartClaw Panel primary lacks unique seat terminal");
    };
    if promotion.terminal_event_id != terminal.event_id
        || promotion.delivery_event_id != delivery.event_id
        || delivery.kind != "ReviewDelivered"
        || delivery.actor != "runtime:orch"
        || event_payload_str(delivery, "role") != Some("primary")
        || event_payload_str(delivery, "agent") != Some("executor-claw")
        || seat.state != "pass"
        || seat.terminal_event_id != terminal.event_id
        || seat.delivery_event_id.as_deref() != Some(delivery.event_id.as_str())
        || bound_primary.delivery_event_id.as_deref() != Some(delivery.event_id.as_str())
        || !(terminal_position < promotion_position
            && promotion_position < delivery_position
            && delivery_position < seat_position)
    {
        bail!("SmartClaw Panel fixed-primary terminal/promotion/delivery/seat chain drifted");
    }
    Ok(())
}

fn required_file_bindings(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    head_sha: &str,
    main_sha: &str,
    implementer_agent: &str,
    ir_task: &crate::plan::IrTask,
    verdict: RootVerdict,
    prospective_root: bool,
) -> Result<(
    Vec<ReviewBinding>,
    Vec<EvidenceBinding>,
    Vec<ReviewSeatSubstitutionBinding>,
)> {
    if crate::round::contract_schema_from_events(events, round)?
        == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        if implementer_agent != "local" {
            bail!("schema 3 review facts 要求 implementer=local");
        }
        let reviews = crate::generic_review::validate_generic_review_facts_for_verdict(
            root,
            events,
            round,
            task_id,
            attempt_id,
            head_sha,
            main_sha,
            verdict,
            prospective_root,
        )?;
        let mut evidence = Vec::new();
        if verdict == RootVerdict::Pass {
            for evidence_id in &ir_task.required_evidence {
                let rel =
                    format!("coordination/rounds/{round}/evidence/{task_id}-{evidence_id}.json");
                let bytes = regular_file_bytes(root, &root.join(&rel), "required evidence")?;
                let committed =
                    committed_regular_blob_bytes(root, main_sha, &rel, "required evidence")?;
                if bytes != committed {
                    bail!("required evidence 当前 bytes 与 expected main blob 不一致: {rel}");
                }
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
        return Ok((reviews, evidence, Vec::new()));
    }
    let mode = review_contract_mode_for_attempt(root, round, events, task_id, attempt_id, ir_task)?;
    if mode == ReviewContractMode::Panel {
        let result = panel_file_bindings(
            root,
            events,
            round,
            task_id,
            attempt_id,
            head_sha,
            main_sha,
            implementer_agent,
            ir_task,
            verdict,
        )?;
        enforce_signed_smartclaw_fixed_primary_pass_v1(
            root, events, round, task_id, attempt_id, head_sha, ir_task, verdict, &result.0,
        )?;
        return Ok(result);
    }
    if mode == ReviewContractMode::Quorum {
        let result = quorum_file_bindings(
            root,
            events,
            round,
            task_id,
            attempt_id,
            head_sha,
            main_sha,
            implementer_agent,
            ir_task,
            verdict,
        )?;
        enforce_signed_smartclaw_fixed_primary_pass_v1(
            root, events, round, task_id, attempt_id, head_sha, ir_task, verdict, &result.0,
        )?;
        return Ok(result);
    }
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
            delivery_event_id: None,
            substituted_role: None,
            substituted_agent: None,
            source_terminal_event_id: None,
        });
    }

    let mut evidence = Vec::new();
    if verdict == RootVerdict::Pass {
        evidence.reserve(ir_task.required_evidence.len());
        for evidence_id in &ir_task.required_evidence {
            let rel = format!("coordination/rounds/{round}/evidence/{task_id}-{evidence_id}.json");
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
    enforce_signed_smartclaw_fixed_primary_pass_v1(
        root, events, round, task_id, attempt_id, head_sha, ir_task, verdict, &reviews,
    )?;
    Ok((reviews, evidence, Vec::new()))
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
/// one explicitly signed attempt permit either to bridge the round's legacy
/// validation marker to its *first* production validation after collect, or
/// to ratify one already-collected production attempt after a card replan. The
/// latter is bound to the dispatch base's committed ledger, IR, and task card
/// and is single-use because no prior root/merge/record or second dispatch may
/// exist.
#[allow(clippy::too_many_arguments)]
fn validate_production_replan_ratification(
    root: &Path,
    prefix: &[EventRecord],
    round: &str,
    task_id: &str,
    ir_revision: u32,
    attempt_id: &str,
    lineage: &AttemptCollect<'_>,
    production: &[(usize, crate::plan::TaskValidatedPayload)],
    production_position: usize,
    signoff_position: usize,
) -> Result<()> {
    let (old_production_position, old_production) = production
        .iter()
        .filter(|(position, _)| *position < lineage.dispatch_position)
        .next_back()
        .context("bootstrap authorization 缺 prior canonical-schema legacy TaskValidated")?;

    if ir_revision <= old_production.ir_revision {
        bail!("production-replan current revision 必须严格大于 dispatch-base revision");
    }
    if !(lineage.dispatch_position < lineage.receipt_position
        && lineage.receipt_position < lineage.collect_position
        && lineage.collect_position < production_position
        && production_position < signoff_position
        && signoff_position < prefix.len())
    {
        bail!("production-replan authorization order 非 canonical ratification chain");
    }

    let dispatches = prefix
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "DispatchIssued" && event.task_id.as_deref() == Some(task_id)
        })
        .collect::<Vec<_>>();
    if dispatches.len() != 1 || dispatches[0].0 != lineage.dispatch_position {
        bail!("production-replan 要求 task 恰好一条 current DispatchIssued");
    }
    let dispatch = dispatches[0].1;
    if dispatch.actor != "runtime:orch" || dispatch.round.as_deref() != Some(round) {
        bail!("production-replan DispatchIssued envelope 非 canonical runtime tuple");
    }
    let dispatch_payload = dispatch
        .payload
        .as_ref()
        .context("production-replan DispatchIssued 缺 payload")?;
    let base_sha = dispatch_payload
        .get("baseSha")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("production-replan DispatchIssued 缺 baseSha")?;
    full_sha(base_sha, "production-replan DispatchIssued.baseSha")?;
    if dispatch_payload
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        != Some(attempt_id)
        || dispatch_payload
            .get("attemptNo")
            .and_then(serde_json::Value::as_u64)
            != Some(lineage.attempt_no as u64)
        || dispatch_payload
            .get("agent")
            .and_then(serde_json::Value::as_str)
            != Some(lineage.implementer_agent.as_str())
    {
        bail!("production-replan DispatchIssued attempt/agent identity 不匹配");
    }
    let current_main = crate::gitx::rev_parse(root, "refs/heads/main")?;
    if !crate::gitx::is_ancestor(root, base_sha, &current_main)? {
        bail!("production-replan DispatchIssued.baseSha 不是 current main ancestor");
    }

    let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let base_ledger_bytes = committed_regular_blob_bytes(
        root,
        base_sha,
        &ledger_rel,
        "production-replan dispatch-base ledger",
    )?;
    let base_events = parse_strict_committed_ledger(
        &base_ledger_bytes,
        "production-replan dispatch-base ledger",
    )?;
    if base_events.len() != lineage.dispatch_position
        || !event_values_equal(&base_events, &prefix[..lineage.dispatch_position])?
    {
        bail!("production-replan dispatch-base ledger 未逐事件绑定 dispatch 前缀");
    }

    if prefix[*old_production_position + 1..lineage.dispatch_position]
        .iter()
        .any(|event| event.kind == "TaskValidated")
    {
        bail!("production-replan old production 后、dispatch 前存在另一 validation");
    }
    let old_signoffs = crate::plan::matching_user_plan_signoff_positions(
        &prefix[..lineage.dispatch_position],
        round,
        old_production.ir_revision,
        &old_production.validation_digest,
    )?;
    if old_signoffs.len() != 1
        || !(*old_production_position < old_signoffs[0]
            && old_signoffs[0] < lineage.dispatch_position)
    {
        bail!("production-replan dispatch-base production tuple 缺唯一 pre-dispatch signoff");
    }
    if prefix[old_signoffs[0] + 1..lineage.dispatch_position]
        .iter()
        .any(|event| event.kind == "PlanSignedOff")
    {
        bail!("production-replan old signoff 后、dispatch 前存在另一 signoff");
    }

    let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let base_ir_bytes = committed_regular_blob_bytes(
        root,
        base_sha,
        &ir_rel,
        "production-replan dispatch-base ROUND-IR",
    )?;
    let base_ir_text = std::str::from_utf8(&base_ir_bytes)
        .context("production-replan dispatch-base ROUND-IR 非 UTF-8")?;
    let base_ir = crate::plan::parse_signed_round_ir(base_ir_text)
        .map_err(anyhow::Error::msg)
        .context("production-replan dispatch-base ROUND-IR 非 canonical")?;
    if base_ir.round != round
        || base_ir.revision != old_production.ir_revision
        || crate::plan::validation_digest(&base_ir) != old_production.validation_digest
    {
        bail!("production-replan dispatch-base ROUND-IR 未绑定 old production tuple");
    }
    let base_tasks = base_ir
        .tasks
        .iter()
        .filter(|task| task.id == task_id)
        .collect::<Vec<_>>();
    if base_tasks.len() != 1 {
        bail!("production-replan dispatch-base ROUND-IR task membership 非唯一");
    }
    let base_task = base_tasks[0];
    if base_task.bootstrap_pre_signoff_attempt.is_some()
        || base_task.agent != lineage.implementer_agent
    {
        bail!("production-replan dispatch-base task permit/agent 不满足 genesis contract");
    }
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let bound_card_sha256 = base_ir
        .source_bindings
        .task_cards
        .get(&card_rel)
        .with_context(|| {
            format!("production-replan dispatch-base ROUND-IR 未绑定 task card: {card_rel}")
        })?;
    let base_card_bytes = committed_regular_blob_bytes(
        root,
        base_sha,
        &card_rel,
        "production-replan dispatch-base task card",
    )?;
    let (actual_card_sha256, _) = sha_binding(&base_card_bytes);
    if &actual_card_sha256 != bound_card_sha256 {
        bail!("production-replan dispatch-base task card SHA 未绑定 old IR");
    }
    let base_card_text = std::str::from_utf8(&base_card_bytes)
        .context("production-replan dispatch-base task card 非 UTF-8")?;
    let base_card = card::parse(&card_rel, task_id, base_card_text)?;
    if base_card.meta.bootstrap_pre_signoff_attempt.is_some()
        || base_card.meta.agent.as_deref() != Some(lineage.implementer_agent.as_str())
    {
        bail!("production-replan dispatch-base card permit/agent 不满足 genesis contract");
    }

    let blocks = prefix
        .iter()
        .enumerate()
        .skip(lineage.dispatch_position + 1)
        .filter(|(_, event)| {
            event.kind == "AttemptBlocked" && event.task_id.as_deref() == Some(task_id)
        })
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        bail!("production-replan 缺 exact runtime AttemptBlocked");
    }
    for (position, block) in &blocks {
        let payload = block
            .payload
            .as_ref()
            .context("production-replan AttemptBlocked 缺 payload")?;
        if block.actor != "runtime:orch"
            || block.round.as_deref() != Some(round)
            || *position >= lineage.receipt_position
            || payload.get("attemptId").and_then(serde_json::Value::as_str) != Some(attempt_id)
            || payload.get("attemptNo").and_then(serde_json::Value::as_u64)
                != Some(lineage.attempt_no as u64)
            || payload.get("agent").and_then(serde_json::Value::as_str)
                != Some(lineage.implementer_agent.as_str())
        {
            bail!("production-replan AttemptBlocked identity/order 非 exact runtime tuple");
        }
    }
    if blocks.last().map(|(position, _)| *position) >= Some(lineage.receipt_position) {
        bail!("production-replan receipt 必须晚于 last exact AttemptBlocked");
    }

    if prefix[lineage.dispatch_position + 1..].iter().any(|event| {
        event.task_id.as_deref() == Some(task_id)
            && matches!(
                event.kind.as_str(),
                "AttemptCrashed" | "AttemptTimedOut" | "AttemptFailed"
            )
    }) {
        bail!("production-replan dispatch..root 含 crash/timeout/fail terminal");
    }
    if prefix.iter().any(|event| {
        event.task_id.as_deref() == Some(task_id)
            && ((event.kind == "VerdictIssued" && event.actor == "verifier:root")
                || matches!(
                    event.kind.as_str(),
                    "MergeStarted" | "MergeExecuted" | "TaskRecorded"
                ))
    }) {
        bail!("production-replan permit 已有 prior root/merge/record，拒绝复用");
    }

    let post_collect_signoffs = prefix
        .iter()
        .enumerate()
        .skip(lineage.collect_position + 1)
        .filter(|(_, event)| event.kind == "PlanSignedOff" && event.round.as_deref() == Some(round))
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    if post_collect_signoffs != vec![signoff_position] {
        bail!("production-replan 要求唯一 post-collect current PlanSignedOff");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_root_authorization_order(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
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
    if !legacy.is_empty() {
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
        return Ok(());
    }

    validate_production_replan_ratification(
        root,
        prefix,
        round,
        task_id,
        ir_revision,
        attempt_id,
        lineage,
        &production,
        production_position,
        signoff_position,
    )
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
    if ir.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        if agent != "local" {
            bail!("schema 3 current implementer 必须是 durable local identity");
        }
        return Ok(());
    }
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

fn is_root_manual_ir(ir: &crate::plan::RoundIr) -> bool {
    ir.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION
        || ir.verification.mode == "root-manual-fixed-head"
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

fn root_verdict_raw_gate_tag(task_id: &str, attempt_id: &str) -> String {
    format!("{task_id}-{attempt_id}-root-verdict")
}

fn root_verdict_scoped_gate_tag(round: &str, task_id: &str, attempt_id: &str) -> String {
    gate::round_scoped_log_tag(round, &root_verdict_raw_gate_tag(task_id, attempt_id))
}

fn legacy_root_verdict_gate_candidates(
    log_dir: &Path,
    task_id: &str,
    attempt_id: &str,
    gate_name: &str,
) -> Result<Vec<PathBuf>> {
    let raw_tag = root_verdict_raw_gate_tag(task_id, attempt_id);
    let suffix = format!("-gate-{gate_name}.log");
    let entries = match fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("读取 root verdict gate 日志目录失败: {}", log_dir.display())
            })
        }
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(middle) = name
            .strip_prefix(&raw_tag)
            .and_then(|rest| rest.strip_suffix(&suffix))
        else {
            continue;
        };
        // Any round-bearing name belongs to the canonical namespace.  A different round must not
        // be reinterpreted as a historical unscoped candidate for this replay.
        if middle.contains("-round-") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).with_context(|| {
            format!(
                "读取 legacy root verdict gate candidate 失败: {}",
                path.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            bail!(
                "legacy root verdict gate candidate 不是 regular file: {}",
                path.display()
            );
        }
        candidates.push(path);
    }
    candidates.sort();
    Ok(candidates)
}

fn bound_root_verdict_gate_log_bytes(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    gate_name: &str,
    expected_sha256: &str,
    expected_len: u64,
) -> Result<Vec<u8>> {
    let log_dir = root.join("coordination/runtime/logs");
    let scoped_path = log_dir.join(format!(
        "{}-gate-{gate_name}.log",
        root_verdict_scoped_gate_tag(round, task_id, attempt_id)
    ));
    let scoped = match fs::symlink_metadata(&scoped_path) {
        Ok(metadata) if metadata.file_type().is_file() => Some(scoped_path),
        Ok(_) => bail!(
            "scoped root verdict gate log 不是 regular file: {}",
            scoped_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "读取 scoped root verdict gate log 失败: {}",
                    scoped_path.display()
                )
            })
        }
    };
    let legacy = legacy_root_verdict_gate_candidates(&log_dir, task_id, attempt_id, gate_name)?;
    let selected = match (scoped, legacy.as_slice()) {
        (Some(_), [_, ..]) => {
            bail!("scoped 与 legacy root verdict gate log 同时存在，拒绝歧义恢复")
        }
        (Some(path), []) => path,
        (None, [path]) => path.clone(),
        (None, []) => bail!("root verdict gate log 无 scoped/legacy candidate: {gate_name}"),
        (None, candidates) => bail!(
            "root verdict gate log 有 {} 个 legacy candidates，拒绝猜测: {gate_name}",
            candidates.len()
        ),
    };
    let bytes = regular_file_bytes(root, &selected, "root verdict gate log")?;
    let (sha256, len) = sha_binding(&bytes);
    if sha256 != expected_sha256 || len != expected_len {
        bail!("既有 root verdict gate log bytes 已漂移: {gate_name}");
    }
    Ok(bytes)
}

fn bound_phase_scoped_root_gate_log_bytes(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    gate_run_id: &str,
    gate_name: &str,
    expected_sha256: &str,
    expected_len: u64,
) -> Result<Vec<u8>> {
    let tag = gate::phase_scoped_log_tag(
        round,
        task_id,
        attempt_id,
        gate::GatePhase::Root,
        gate_run_id,
    );
    let path = root
        .join("coordination/runtime/logs")
        .join(format!("{tag}-gate-{gate_name}.log"));
    let bytes = regular_file_bytes(root, &path, "phase-scoped root verdict gate log")?;
    let (sha256, len) = sha_binding(&bytes);
    if sha256 != expected_sha256 || len != expected_len {
        bail!("phase-scoped root verdict gate log bytes 已漂移: {gate_name}");
    }
    Ok(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedRootGateSubjectV1 {
    subject: RootGateSubjectV1,
    ordered_command_refs: Vec<String>,
    policy_base_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedRootGateReuseV1 {
    bundle: CollectGateBundleV1,
    subject: RootGateSubjectV1,
    gates: Vec<ReusedGateV1>,
    input_identity_sha256: String,
    attempt_no: usize,
    policy_base_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootGateReuseMissV1 {
    reason: String,
    detail: String,
    command_ref: String,
    input_identity_sha256: String,
    expected_sha256: String,
    actual_sha256: String,
    policy_base_sha: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LiveRootGateReusePlanV1 {
    Reuse(PreparedRootGateReuseV1),
    Execute(RootGateReuseMissV1),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RootGateReuseInputIdentityV1<'a> {
    contract_version: u32,
    attempt_id: &'a str,
    policy_base_sha: &'a str,
    bundle: &'a CollectGateBundleV1,
    subject: &'a RootGateSubjectV1,
}

fn root_gate_reuse_json_sha256<T: Serialize>(domain: &str, value: &T) -> Result<String> {
    let bytes = serde_json::to_vec(value).context("root gate reuse identity JSON 编码失败")?;
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain.as_bytes());
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn attempt_runtime_policy_active(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
    policy: &str,
) -> Result<bool> {
    match crate::plan::resolve_attempt_runtime_policy(
        root, round, events, task_id, attempt_id, policy,
    ) {
        Ok(resolution) => Ok(resolution.state == crate::plan::RuntimePolicyStateV1::Active),
        Err(error)
            if error
                .to_string()
                .contains(&format!("缺 runtime policy {policy}"))
                || error.to_string().contains("缺 runtimePolicies envelope")
                || {
                    let detail = format!("{error:#}");
                    (detail.contains("读取 policy-base committed PROJECT-BINDING 失败")
                        || detail.contains("读取 policy-base committed ROUND-IR 失败"))
                        && (detail.contains("does not exist in")
                            || detail.contains("exists on disk, but not in"))
                } =>
        {
            Ok(false)
        }
        Err(error) => {
            Err(error).with_context(|| format!("resolve attempt runtime policy {policy} failed"))
        }
    }
}

fn root_gate_reuse_policy_active(
    root: &Path,
    round: &str,
    events: &[EventRecord],
    task_id: &str,
    attempt_id: &str,
) -> Result<bool> {
    if crate::round::contract_schema_from_events(events, round)?
        == Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        return Ok(false);
    }
    attempt_runtime_policy_active(root, round, events, task_id, attempt_id, "root-reuse-v1")
}

fn capture_root_gate_subject_v1(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
    active: &crate::plan::ReadonlyIrValidation,
    events: &[EventRecord],
) -> Result<CapturedRootGateSubjectV1> {
    let dispatch = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    if dispatch.attempt_id.as_deref() != Some(attempt_id) {
        bail!("root reuse subject attempt 不是 durable current DispatchIssued");
    }
    let policy_base_sha = dispatch
        .base_sha
        .context("root reuse subject DispatchIssued 缺 baseSha")?;
    full_sha(&policy_base_sha, "root reuse policyBaseSha")?;
    let bound_card = crate::plan::load_bound_task_card(root, round, task_id, active)?;
    let lane_plan = collect::resolve_collect_lane_plan_at_candidate(
        root,
        round,
        &bound_card,
        events,
        attempt_id,
        &policy_base_sha,
        expected_head,
    )?;
    let task_card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let task_card_sha256 = active
        .candidate
        .source_bindings
        .task_cards
        .get(&task_card_rel)
        .with_context(|| format!("active ROUND-IR 未绑定 task card: {task_card_rel}"))?
        .clone();
    let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
    Ok(CapturedRootGateSubjectV1 {
        subject: RootGateSubjectV1 {
            subject_tree_sha: crate::gitx::rev_parse(root, &format!("{expected_head}^{{tree}}"))?,
            ir_revision: active.persisted_revision,
            validation_digest: active.persisted_digest.clone(),
            binding_sha256: active.candidate.source_bindings.binding_sha256.clone(),
            task_card_sha256,
            resolved_command_digest: lane_plan.resolved_command_digest().to_string(),
            toolchain_digest: fingerprint.toolchain_digest,
            environment_digest: fingerprint.environment_digest,
        },
        ordered_command_refs: lane_plan.ordered_command_refs(),
        policy_base_sha,
    })
}

fn project_validated_collect_bundle_v1(
    validated: &crate::tierf::ValidatedCollectGateBundleV1,
) -> CollectGateBundleV1 {
    CollectGateBundleV1 {
        subject_tree_sha: validated.subject_tree_sha.clone(),
        ir_revision: validated.ir_revision,
        validation_digest: validated.validation_digest.clone(),
        binding_sha256: validated.binding_sha256.clone(),
        task_card_sha256: validated.task_card_sha256.clone(),
        resolved_command_digest: validated.resolved_command_digest.clone(),
        toolchain_digest: validated.toolchain_digest.clone(),
        environment_digest: validated.environment_digest.clone(),
        gates: validated
            .gates
            .iter()
            .map(|gate| ReusedGateV1 {
                name: gate.command_ref.clone(),
                exit_code: gate.exit_code,
                source_event_id: gate.source_event_id.clone(),
                log_sha256: gate.log_sha256.clone(),
                log_bytes: gate.log_bytes,
            })
            .collect(),
    }
}

fn root_gate_reuse_miss(
    captured: &CapturedRootGateSubjectV1,
    reason: &str,
    detail: String,
    expected_sha256: String,
) -> Result<LiveRootGateReusePlanV1> {
    let actual_sha256 =
        root_gate_reuse_json_sha256("root-gate-reuse-subject-v1", &captured.subject)?;
    Ok(LiveRootGateReusePlanV1::Execute(RootGateReuseMissV1 {
        reason: reason.to_string(),
        detail,
        command_ref: captured
            .ordered_command_refs
            .first()
            .cloned()
            .unwrap_or_else(|| "root-reuse-v1".to_string()),
        input_identity_sha256: actual_sha256.clone(),
        expected_sha256,
        actual_sha256,
        policy_base_sha: captured.policy_base_sha.clone(),
    }))
}

fn plan_live_root_gate_reuse_v1(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
    attempt_no: usize,
    active: &crate::plan::ReadonlyIrValidation,
    events: &[EventRecord],
) -> Result<LiveRootGateReusePlanV1> {
    let captured = capture_root_gate_subject_v1(
        root,
        round,
        task_id,
        attempt_id,
        expected_head,
        active,
        events,
    )?;
    let validated =
        match crate::tierf::load_validated_collect_gate_bundle_v1(root, round, task_id, attempt_id)
        {
        Ok(Some(bundle)) => bundle,
        Ok(None) => {
            return root_gate_reuse_miss(
                &captured,
                "contract",
                "completed collect receipt is absent".to_string(),
                root_gate_reuse_json_sha256(
                    "root-gate-reuse-missing-receipt-v1",
                    &"completed collect receipt is absent",
                )?,
            )
        }
        Err(error) => {
            let detail = format!("collect receipt/CAS reread failed: {error:#}");
            return root_gate_reuse_miss(
                &captured,
                "contract",
                detail.clone(),
                    root_gate_reuse_json_sha256("root-gate-reuse-unreadable-receipt-v1", &detail)?,
            );
        }
    };
    let bundle = project_validated_collect_bundle_v1(&validated);
    let expected_sha256 = root_gate_reuse_json_sha256("collect-gate-bundle-v1", &bundle)?;
    if validated.candidate_sha != expected_head {
        return root_gate_reuse_miss(
            &captured,
            "input-tree",
            "collect candidate commit differs from expected head".to_string(),
            expected_sha256,
        );
    }
    if validated.attempt_id != attempt_id || validated.policy_base_sha != captured.policy_base_sha {
        return root_gate_reuse_miss(
            &captured,
            "contract",
            "collect attempt or policy base differs from root lineage".to_string(),
            expected_sha256,
        );
    }
    let decision = plan_root_gate_reuse_v1(&bundle, &captured.subject);
    let RootGateReuseDecisionV1::Reuse(gates) = decision else {
        let RootGateReuseDecisionV1::Execute { reason } = decision else {
            unreachable!()
        };
        let detail = match reason.as_str() {
            "input-tree" => format!(
                "subjectTreeSha expected={} actual={}",
                bundle.subject_tree_sha, captured.subject.subject_tree_sha
            ),
            "contract" => format!(
                "contract expected=ir:{}/{} binding:{} card:{} actual=ir:{}/{} binding:{} card:{} gates={:?}",
                bundle.ir_revision,
                bundle.validation_digest,
                bundle.binding_sha256,
                bundle.task_card_sha256,
                captured.subject.ir_revision,
                captured.subject.validation_digest,
                captured.subject.binding_sha256,
                captured.subject.task_card_sha256,
                bundle.gates
            ),
            "command" => format!(
                "resolvedCommandDigest expected={} actual={}",
                bundle.resolved_command_digest, captured.subject.resolved_command_digest
            ),
            "toolchain" => format!(
                "toolchainDigest expected={} actual={}",
                bundle.toolchain_digest, captured.subject.toolchain_digest
            ),
            "environment" => format!(
                "environmentDigest expected={} actual={}",
                bundle.environment_digest, captured.subject.environment_digest
            ),
            _ => format!("collect/root {reason} identity drift"),
        };
        return root_gate_reuse_miss(&captured, &reason, detail, expected_sha256);
    };
    let gate_names = gates
        .iter()
        .map(|gate| gate.name.as_str())
        .collect::<Vec<_>>();
    let expected_names = captured
        .ordered_command_refs
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if gate_names != expected_names {
        return root_gate_reuse_miss(
            &captured,
            "command",
            "collect gate order differs from freshly resolved invocation order".to_string(),
            expected_sha256,
        );
    }
    let input_identity_sha256 = root_gate_reuse_json_sha256(
        "root-gate-reuse-input-v1",
        &RootGateReuseInputIdentityV1 {
            contract_version: ROOT_GATE_REUSE_CONTRACT_V1,
            attempt_id,
            policy_base_sha: &captured.policy_base_sha,
            bundle: &bundle,
            subject: &captured.subject,
        },
    )?;
    Ok(LiveRootGateReusePlanV1::Reuse(PreparedRootGateReuseV1 {
            bundle,
            subject: captured.subject,
            gates,
            input_identity_sha256,
            attempt_no,
            policy_base_sha: captured.policy_base_sha,
    }))
}

fn append_root_gate_reuse_miss_once(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    attempt_no: usize,
    miss: &RootGateReuseMissV1,
) -> Result<()> {
    let miss = miss.clone();
    let appended = ledger::append_checked(root, round, |events| {
        let prior = events
            .iter()
            .filter_map(|event| {
                let Ok(Some(ledger::RuntimeEventPayloadV1::GateReuseMiss(payload))) =
                    ledger::decode_runtime_event_v1(event)
                else {
                    return None;
                };
                (event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round)
                    && payload.attempt_id == attempt_id)
                    .then_some(payload)
            })
            .collect::<Vec<_>>();
        if prior.iter().any(|payload| {
            payload.phase == "root"
                && payload.reason == miss.reason
                && payload.command_ref == miss.command_ref
                && payload.input_identity_sha256 == miss.input_identity_sha256
                && payload.expected_sha256 == miss.expected_sha256
                && payload.actual_sha256 == miss.actual_sha256
                && payload.policy_base_sha == miss.policy_base_sha
        }) {
            return Ok(Vec::new());
        }
        let miss_no = u32::try_from(prior.len() + 1).context("root reuse miss count 溢出")?;
        if miss_no > 2 {
            bail!("root reuse miss budget exhausted for {attempt_id}");
        }
        Ok(vec![ledger::runtime_event_v1(
            round,
            Some(task_id),
            ledger::RuntimeEventPayloadV1::GateReuseMiss(ledger::GateReuseMissPayloadV1 {
                schema_version: 1,
                attempt_id: attempt_id.to_string(),
                attempt_no,
                phase: "root".to_string(),
                command_ref: miss.command_ref.clone(),
                miss_no,
                reason: miss.reason.clone(),
                input_identity_sha256: miss.input_identity_sha256.clone(),
                expected_sha256: miss.expected_sha256.clone(),
                actual_sha256: miss.actual_sha256.clone(),
                policy_base_sha: miss.policy_base_sha.clone(),
            }),
        )?])
    })?;
    if appended > 1 {
        bail!("GateReuseMiss append count 非 canonical: {appended}");
    }
    eprintln!(
        "[orch] root gate reuse miss: reason={} detail={}",
        miss.reason, miss.detail
    );
    Ok(())
}

fn source_gate_duration_ms(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    prepared: &PreparedRootGateReuseV1,
    gate: &ReusedGateV1,
) -> Result<u64> {
    let sources = events
        .iter()
        .filter(|event| event.event_id == gate.source_event_id)
        .collect::<Vec<_>>();
    let [source] = sources.as_slice() else {
        bail!("root reuse source GateExecuted eventId 必须唯一");
    };
    let payload = canonical_gate_executed_payload(source, round)
        .context("root reuse source 不是 canonical GateExecuted")?;
    if source.task_id.as_deref() != Some(task_id)
        || payload.get("phase").and_then(serde_json::Value::as_str) != Some("collect")
        || payload
            .get("commandRef")
            .and_then(serde_json::Value::as_str)
            != Some(gate.name.as_str())
        || payload.get("exitCode").and_then(serde_json::Value::as_i64)
            != Some(i64::from(gate.exit_code))
        || payload
            .get("subjectTreeSha")
            .and_then(serde_json::Value::as_str)
            != Some(prepared.subject.subject_tree_sha.as_str())
        || payload.get("logSha256").and_then(serde_json::Value::as_str)
            != Some(gate.log_sha256.as_str())
        || payload.get("logBytes").and_then(serde_json::Value::as_u64) != Some(gate.log_bytes)
        || payload
            .get("toolchainDigest")
            .and_then(serde_json::Value::as_str)
            != Some(prepared.bundle.toolchain_digest.as_str())
        || payload
            .get("environmentDigest")
            .and_then(serde_json::Value::as_str)
            != Some(prepared.bundle.environment_digest.as_str())
    {
        bail!("root reuse source GateExecuted bytes/identity 漂移");
    }
    payload
        .get("durationMs")
        .and_then(serde_json::Value::as_u64)
        .context("root reuse source GateExecuted 缺 durationMs")
}

fn build_root_gate_reused_events(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    prepared: &PreparedRootGateReuseV1,
) -> Result<Vec<EventRecord>> {
    prepared
        .gates
        .iter()
        .map(|gate| {
            let saved_ms = source_gate_duration_ms(events, round, task_id, prepared, gate)?;
            ledger::runtime_event_v1(
                round,
                Some(task_id),
                ledger::RuntimeEventPayloadV1::GateReused(ledger::GateReusedPayloadV1 {
                    schema_version: 1,
                    attempt_id: attempt_id.to_string(),
                    attempt_no: prepared.attempt_no,
                    source_gate_event_id: gate.source_event_id.clone(),
                    source_phase: "collect".to_string(),
                    target_phase: "root".to_string(),
                    input_identity_sha256: prepared.input_identity_sha256.clone(),
                    command_ref: gate.name.clone(),
                    subject_tree_sha: prepared.subject.subject_tree_sha.clone(),
                    log_sha256: gate.log_sha256.clone(),
                    log_bytes: gate.log_bytes,
                    saved_ms,
                }),
            )
        })
        .collect()
}

fn root_gate_reused_outputs(
    root: &Path,
    prepared: &PreparedRootGateReuseV1,
    reused_events: &[EventRecord],
) -> Result<(Vec<gate::GateResult>, Vec<VerdictGateBinding>)> {
    if reused_events.len() != prepared.gates.len() {
        bail!("GateReused event count 与 planned source gates 不一致");
    }
    let store = crate::cas::Store::new(&root.join("coordination/runtime/cas"));
    let results = prepared
        .gates
        .iter()
        .map(|gate| gate::GateResult {
            name: gate.name.clone(),
            exit_code: gate.exit_code,
            duration_ms: 0,
            log_path: store.object_path(&gate.log_sha256).display().to_string(),
        })
        .collect::<Vec<_>>();
    let bindings = prepared
        .gates
        .iter()
        .zip(reused_events)
        .map(|(gate, event)| VerdictGateBinding {
            name: gate.name.clone(),
            exit_code: gate.exit_code,
            log_sha256: gate.log_sha256.clone(),
            log_bytes: gate.log_bytes,
            gate_run_id: None,
            reused_event_id: Some(event.event_id.clone()),
        })
        .collect();
    Ok((results, bindings))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RootReuseCollectAttestedGateV1 {
    sequence: u64,
    gate_event_id: String,
    gate_run_id: String,
    command_ref: String,
    exit_code: i32,
    duration_ms: u64,
    raw_log_cas_sha256: String,
    raw_log_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RootReuseCollectAttestationV1 {
    attestation_version: u64,
    round: String,
    task_id: String,
    action_id: String,
    owner: String,
    lease_generation: String,
    attempt_id: String,
    attempt_no: u64,
    agent: String,
    base_sha: String,
    go_path: String,
    executing_event_id: String,
    branch_sha: String,
    evidence_path: String,
    evidence_sha256: String,
    evidence_len: u64,
    control_epoch: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    subject_tree_sha: String,
    #[serde(default, skip_serializing_if = "root_reuse_zero_u32")]
    ir_revision: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    validation_digest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    binding_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    task_card_sha256: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    resolved_command_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_reader_descriptor_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_reader_base_sha256: Option<String>,
    toolchain_digest: String,
    environment_digest: String,
    configured_gate_count: u64,
    configured_command_refs: Vec<String>,
    gates: Vec<RootReuseCollectAttestedGateV1>,
}

fn root_reuse_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn read_root_reuse_collect_attestation(
    root: &Path,
    receipt: &EventRecord,
) -> Result<RootReuseCollectAttestationV1> {
    let payload = receipt
        .payload
        .as_ref()
        .context("root reuse CollectGateSuccessReceipt 缺 payload")?;
    let sha256 = payload
        .get("receiptAttestationSha256")
        .and_then(serde_json::Value::as_str)
        .context("root reuse receipt 缺 receiptAttestationSha256")?;
    let expected_len = payload
        .get("receiptAttestationLen")
        .and_then(serde_json::Value::as_u64)
        .context("root reuse receipt 缺 receiptAttestationLen")?;
    full_sha256(sha256, "root reuse receipt attestation SHA-256")?;
    let store = crate::cas::Store::new(&root.join("coordination/runtime/cas"));
    let bytes = store
        .get(sha256)?
        .with_context(|| format!("root reuse receipt attestation CAS 缺失: {sha256}"))?;
    let (actual_sha256, actual_len) = sha_binding(&bytes);
    if actual_sha256 != sha256 || actual_len != expected_len {
        bail!("root reuse receipt attestation CAS bytes 漂移");
    }
    let attestation: RootReuseCollectAttestationV1 = serde_json::from_slice(&bytes)
        .context("root reuse receipt attestation 非 canonical JSON")?;
    if serde_json::to_vec(&attestation)? != bytes
        || attestation.attestation_version != 2
        || payload
            .get("receiptVersion")
            .and_then(serde_json::Value::as_u64)
            != Some(3)
        || payload.get("gates") != Some(&serde_json::to_value(&attestation.gates)?)
        || payload.get("configuredCommandRefs")
            != Some(&serde_json::to_value(&attestation.configured_command_refs)?)
        || payload
            .get("resolvedCommandDigest")
            .and_then(serde_json::Value::as_str)
            != Some(attestation.resolved_command_digest.as_str())
        || payload
            .get("toolchainDigest")
            .and_then(serde_json::Value::as_str)
            != Some(attestation.toolchain_digest.as_str())
        || payload
            .get("environmentDigest")
            .and_then(serde_json::Value::as_str)
            != Some(attestation.environment_digest.as_str())
    {
        bail!("root reuse receipt ledger binding 与 attestation CAS 不一致");
    }
    Ok(attestation)
}

fn root_reuse_collect_attestation_from_committed_receipt(
    events: &[EventRecord],
    receipt_position: usize,
) -> Result<RootReuseCollectAttestationV1> {
    let receipt = events.get(receipt_position)
        .context("archived root reuse receipt position 越界")?;
    if receipt.kind != "CollectGateSuccessReceipt" || receipt.actor != "runtime:orch" {
        bail!("archived root reuse receipt identity/actor 非 canonical");
    }
    let payload = receipt
        .payload
        .as_ref()
        .context("archived root reuse CollectGateSuccessReceipt 缺 payload")?;
    let string = |key: &str| -> Result<String> {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .with_context(|| format!("archived root reuse receipt 缺 {key}"))
    };
    let unsigned = |key: &str| -> Result<u64> {
        payload
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .with_context(|| format!("archived root reuse receipt 缺 {key}"))
    };
    if unsigned("receiptVersion")? != 3 {
        bail!("archived root reuse receiptVersion 非 3");
    }
    let attestation_sha256 = string("receiptAttestationSha256")?;
    full_sha256(
        &attestation_sha256,
        "archived root reuse receiptAttestationSha256",
    )?;
    if string("receiptDigest")? != attestation_sha256
        || string("gateReceiptDigest")? != attestation_sha256
        || unsigned("receiptAttestationLen")? == 0
    {
        bail!("archived root reuse receipt digest/length 未闭合");
    }
    let gates: Vec<RootReuseCollectAttestedGateV1> = serde_json::from_value(
        payload
            .get("gates")
            .cloned()
            .context("archived root reuse receipt 缺 gates")?,
    )
    .context("archived root reuse receipt gates 非 canonical")?;
    let configured_command_refs: Vec<String> = serde_json::from_value(
        payload
            .get("configuredCommandRefs")
            .cloned()
            .context("archived root reuse receipt 缺 configuredCommandRefs")?,
    )
    .context("archived root reuse configuredCommandRefs 非 canonical")?;
    if unsigned("gateCount")? != gates.len() as u64
        || configured_command_refs.len() != gates.len()
        || gates.iter().enumerate().any(|(index, gate)| {
            gate.sequence != index as u64 + 1
                || gate.command_ref != configured_command_refs[index]
                || gate.exit_code != 0
                || gate.raw_log_len == 0
                || full_sha256(&gate.raw_log_cas_sha256, "archived gate raw log").is_err()
        })
    {
        bail!("archived root reuse receipt gate list/order 非 canonical");
    }
    let executing_matches = events[..receipt_position].iter().filter(|event| {
        event.kind == "ReportCollectExecuting"
            && event.actor == "runtime:orch"
            && event.round == receipt.round
            && event.task_id == receipt.task_id
            && event.payload.as_ref().is_some_and(|executing| {
                ["actionId", "owner", "leaseGeneration", "attemptId", "attemptNo", "agent",
                    "baseSha", "goPath", "evidencePath", "evidenceSha256", "evidenceLen",
                    "controlEpoch"].iter()
                    .all(|key| executing.get(*key) == payload.get(*key))
            })
    }).collect::<Vec<_>>();
    let [executing] = executing_matches.as_slice() else {
        bail!("archived root reuse receipt 缺唯一 preceding Executing lineage");
    };
    if events.iter().filter(|event| event.event_id == executing.event_id).count() != 1 {
        bail!("archived root reuse Executing eventId 不唯一");
    }
    let source_reader_pair = match (payload.get("sourceReaderDescriptorSha256"), payload.get("sourceReaderBaseSha256")) {
        (None, None) => (None, None),
        (Some(serde_json::Value::String(descriptor)), Some(serde_json::Value::String(base))) => {
            full_sha256(descriptor, "archived source-reader descriptor")?;
            full_sha256(base, "archived source-reader base")?;
            (Some(descriptor.clone()), Some(base.clone()))
        }
        _ => bail!("archived root reuse source-reader digest pair 非 canonical"),
    };
    let attestation = RootReuseCollectAttestationV1 {
        attestation_version: 2,
        round: receipt
            .round
            .clone()
            .context("archived root reuse receipt 缺 round")?,
        task_id: receipt
            .task_id
            .clone()
            .context("archived root reuse receipt 缺 taskId")?,
        action_id: string("actionId")?,
        owner: string("owner")?,
        lease_generation: string("leaseGeneration")?,
        attempt_id: string("attemptId")?,
        attempt_no: unsigned("attemptNo")?,
        agent: string("agent")?,
        base_sha: string("baseSha")?,
        go_path: string("goPath")?,
        executing_event_id: executing.event_id.clone(),
        branch_sha: string("branchSha")?,
        evidence_path: string("evidencePath")?,
        evidence_sha256: string("evidenceSha256")?,
        evidence_len: unsigned("evidenceLen")?,
        control_epoch: string("controlEpoch")?,
        subject_tree_sha: string("subjectTreeSha")?,
        ir_revision: u32::try_from(unsigned("irRevision")?)
            .context("archived root reuse irRevision 溢出")?,
        validation_digest: string("validationDigest")?,
        binding_sha256: string("bindingSha256")?,
        task_card_sha256: string("taskCardSha256")?,
        resolved_command_digest: string("resolvedCommandDigest")?,
        source_reader_descriptor_sha256: source_reader_pair.0,
        source_reader_base_sha256: source_reader_pair.1,
        toolchain_digest: string("toolchainDigest")?,
        environment_digest: string("environmentDigest")?,
        configured_gate_count: gates.len() as u64,
        configured_command_refs,
        gates,
    };
    let (actual_sha256, actual_len) = sha_binding(&serde_json::to_vec(&attestation)?);
    if actual_sha256 != attestation_sha256 || actual_len != unsigned("receiptAttestationLen")? {
        bail!("archived root reuse receipt attestation committed bytes 漂移");
    }
    Ok(attestation)
}

#[cfg(test)]
mod archived_root_reuse_attestation_tests {
    use super::*;

    #[test]
    fn committed_receipt_reconstructs_exact_attestation_and_rejects_tampering() {
        // Historical event bytes are read-only evidence, not generated through
        // a retired writer or loaded from mutable runtime CAS.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
        let head = crate::gitx::rev_parse(root, "HEAD").unwrap();
        let bytes = committed_regular_blob_bytes(root, &head,
            "coordination/rounds/r81/events.jsonl", "attestation test history").unwrap();
        let events = std::str::from_utf8(&bytes).unwrap().lines()
            .map(|line| serde_json::from_str::<EventRecord>(line).unwrap())
            .collect::<Vec<_>>();
        let position = events.iter().position(|event| event.kind == "CollectGateSuccessReceipt"
            && event.task_id.as_deref() == Some("B305")).unwrap();
        let attestation = root_reuse_collect_attestation_from_committed_receipt(&events, position).unwrap();
        assert_eq!(attestation.executing_event_id, "01M10N9X2QRPRXCDX3NPTM42YN");
        for (key, value, expected_error) in [
            ("receiptAttestationSha256", serde_json::json!("a".repeat(64)), "digest/length"),
            ("receiptAttestationLen", serde_json::json!(1), "committed bytes"),
            ("sourceReaderDescriptorSha256", serde_json::json!("a".repeat(64)), "committed bytes"),
            ("owner", serde_json::json!("another-owner"), "Executing lineage"),
        ] {
            let mut changed = events.clone();
            changed[position].payload.as_mut().unwrap()[key] = value;
            let error = root_reuse_collect_attestation_from_committed_receipt(&changed, position)
                .unwrap_err().to_string();
            assert!(error.contains(expected_error), "{key}: {error}");
        }
        let executing = events.iter().position(|event| event.event_id == attestation.executing_event_id).unwrap();
        let mut changed = events.clone();
        changed[position].payload.as_mut().unwrap().as_object_mut().unwrap().remove("sourceReaderBaseSha256");
        assert!(root_reuse_collect_attestation_from_committed_receipt(&changed, position)
            .unwrap_err().to_string().contains("source-reader digest pair"));
        changed = events.clone();
        changed[executing].event_id = "01M10N9X2QRPRXCDX3NPTM42YQ".into();
        assert!(root_reuse_collect_attestation_from_committed_receipt(&changed, position)
            .unwrap_err().to_string().contains("committed bytes"));
        changed = events.clone();
        changed.remove(executing);
        assert!(root_reuse_collect_attestation_from_committed_receipt(&changed, position - 1)
            .unwrap_err().to_string().contains("Executing lineage"));
        changed = events.clone();
        changed.insert(executing, events[executing].clone());
        assert!(root_reuse_collect_attestation_from_committed_receipt(&changed, position + 1)
            .unwrap_err().to_string().contains("Executing lineage"));
        changed = events.clone();
        for key in ["receiptAttestationSha256", "receiptDigest", "gateReceiptDigest"] {
            changed[position].payload.as_mut().unwrap()[key] = serde_json::json!("a".repeat(64));
        }
        assert!(root_reuse_collect_attestation_from_committed_receipt(&changed, position)
            .unwrap_err().to_string().contains("committed bytes"));
    }
}

fn validate_reused_gate_event_and_source(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
    reused_position: usize,
    reused_event: &EventRecord,
    binding: &VerdictGateBinding,
    expected_gate: &ReusedGateV1,
    expected_subject: &RootGateSubjectV1,
    expected_input_identity_sha256: &str,
    expected_collect_completed_event_id: &str,
    committed_attestation: Option<&RootReuseCollectAttestationV1>,
) -> Result<RootReuseCollectAttestationV1> {
    if reused_event.task_id.as_deref() != Some(task_id)
        || reused_event.round.as_deref() != Some(round)
        || reused_event.actor != "runtime:orch"
        || binding.reused_event_id.as_deref() != Some(reused_event.event_id.as_str())
        || binding.gate_run_id.is_some()
    {
        bail!("verdict reused gate execution reference 非 exact xor tuple");
    }
    let Some(ledger::RuntimeEventPayloadV1::GateReused(reused)) =
        ledger::decode_runtime_event_v1(reused_event)?
    else {
        bail!("verdict reusedEventId 未引用 typed GateReused");
    };
    if reused.attempt_id != attempt_id
        || reused.source_gate_event_id != expected_gate.source_event_id
        || reused.source_phase != "collect"
        || reused.target_phase != "root"
        || reused.input_identity_sha256 != expected_input_identity_sha256
        || reused.command_ref != binding.name
        || reused.command_ref != expected_gate.name
        || reused.subject_tree_sha != expected_subject.subject_tree_sha
        || reused.log_sha256 != binding.log_sha256
        || reused.log_sha256 != expected_gate.log_sha256
        || reused.log_bytes != binding.log_bytes
        || reused.log_bytes != expected_gate.log_bytes
        || binding.exit_code != 0
        || expected_gate.exit_code != 0
    {
        bail!("GateReused 与 verdict/source identity 漂移");
    }
    let sources = events[..reused_position]
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == reused.source_gate_event_id)
        .collect::<Vec<_>>();
    let [(source_position, source)] = sources.as_slice() else {
        bail!("GateReused sourceGateEventId 在先行历史中不唯一");
    };
    let source_payload = canonical_gate_executed_payload(source, round)
        .context("GateReused source 不是 canonical GateExecuted")?;
    if source.task_id.as_deref() != Some(task_id)
        || source_payload
            .get("phase")
            .and_then(serde_json::Value::as_str)
            != Some("collect")
        || source_payload
            .get("commandRef")
            .and_then(serde_json::Value::as_str)
            != Some(binding.name.as_str())
        || source_payload
            .get("subjectTreeSha")
            .and_then(serde_json::Value::as_str)
            != Some(expected_subject.subject_tree_sha.as_str())
        || source_payload
            .get("logSha256")
            .and_then(serde_json::Value::as_str)
            != Some(binding.log_sha256.as_str())
        || source_payload
            .get("logBytes")
            .and_then(serde_json::Value::as_u64)
            != Some(binding.log_bytes)
        || source_payload
            .get("durationMs")
            .and_then(serde_json::Value::as_u64)
            != Some(reused.saved_ms)
        || source_payload
            .get("exitCode")
            .and_then(serde_json::Value::as_i64)
            != Some(i64::from(expected_gate.exit_code))
        || source_payload
            .get("toolchainDigest")
            .and_then(serde_json::Value::as_str)
            != Some(expected_subject.toolchain_digest.as_str())
        || source_payload
            .get("environmentDigest")
            .and_then(serde_json::Value::as_str)
            != Some(expected_subject.environment_digest.as_str())
    {
        bail!("GateReused source GateExecuted fields 漂移");
    }
    let receipts = events[*source_position + 1..reused_position]
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "CollectGateSuccessReceipt"
                && event.actor == "runtime:orch"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("gates"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|gates| {
                        gates.iter().any(|gate| {
                            gate.get("gateEventId").and_then(serde_json::Value::as_str)
                                == Some(reused.source_gate_event_id.as_str())
                        })
                    })
        })
        .map(|(offset, event)| (*source_position + 1 + offset, event))
        .collect::<Vec<_>>();
    let [(receipt_position, receipt)] = receipts.as_slice() else {
        bail!("GateReused source 未绑定唯一 collect receipt");
    };
    let attestation = match committed_attestation {
        Some(attestation) => attestation.clone(),
        None => read_root_reuse_collect_attestation(root, receipt)?,
    };
    if attestation.round != round
        || attestation.task_id != task_id
        || attestation.attempt_id != attempt_id
        || attestation.subject_tree_sha != expected_subject.subject_tree_sha
        || attestation.ir_revision != expected_subject.ir_revision
        || attestation.validation_digest != expected_subject.validation_digest
        || attestation.binding_sha256 != expected_subject.binding_sha256
        || attestation.task_card_sha256 != expected_subject.task_card_sha256
        || attestation.resolved_command_digest != expected_subject.resolved_command_digest
        || attestation.toolchain_digest != expected_subject.toolchain_digest
        || attestation.environment_digest != expected_subject.environment_digest
        || attestation.configured_gate_count != attestation.gates.len() as u64
        || attestation.configured_command_refs.len() != attestation.gates.len()
    {
        bail!("GateReused collect attestation contract/subject 漂移");
    }
    let attested = attestation
        .gates
        .iter()
        .filter(|gate| gate.gate_event_id == reused.source_gate_event_id)
        .collect::<Vec<_>>();
    let [attested] = attested.as_slice() else {
        bail!("GateReused source 在 attestation 中不唯一");
    };
    if attested.command_ref != binding.name
        || attested.exit_code != 0
        || attested.raw_log_cas_sha256 != binding.log_sha256
        || attested.raw_log_len != binding.log_bytes
        || attested.duration_ms != reused.saved_ms
        || source_payload
            .get("gateRunId")
            .and_then(serde_json::Value::as_str)
            != Some(attested.gate_run_id.as_str())
    {
        bail!("GateReused source/attestation gate tuple 漂移");
    }
    let completed = events[*receipt_position + 1..reused_position]
        .iter()
        .filter(|event| {
            event.kind == "ReportCollectCompleted"
                && event.actor == "runtime:orch"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt_id)
                && event
                    .payload
                    .as_ref()
                    .and_then(|payload| payload.get("gateReceipt"))
                    .and_then(serde_json::Value::as_str)
                    == Some(receipt.event_id.as_str())
        })
        .collect::<Vec<_>>();
    let [completed] = completed.as_slice() else {
        bail!("GateReused source receipt 未绑定唯一 completed collect");
    };
    if completed.event_id != expected_collect_completed_event_id {
        bail!("GateReused source receipt 未绑定 verdict collectCompletedEventId");
    }
    if committed_attestation.is_none() {
        let store = crate::cas::Store::new(&root.join("coordination/runtime/cas"));
        let raw = store
            .get(&binding.log_sha256)?
            .context("GateReused raw-log CAS object 缺失")?;
        let (raw_sha256, raw_len) = sha_binding(&raw);
        if raw_sha256 != binding.log_sha256 || raw_len != binding.log_bytes {
            bail!("GateReused raw-log CAS bytes 漂移");
        }
    }
    Ok(attestation)
}

fn modern_root_execution_observed(
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    attempt_id: &str,
) -> bool {
    events.iter().any(|event| {
        if event.task_id.as_deref() != Some(task_id) || event.round.as_deref() != Some(round) {
            return false;
        }
        if canonical_gate_executed_payload(event, round).is_some_and(|payload| {
            payload.get("phase").and_then(serde_json::Value::as_str) == Some("root")
        }) {
            return true;
        }
        matches!(
            ledger::decode_runtime_event_v1(event),
            Ok(Some(ledger::RuntimeEventPayloadV1::GateReused(ref payload)))
                if payload.attempt_id == attempt_id && payload.target_phase == "root"
        )
    })
}

fn validate_verdict_gate_execution_reference(
    binding: &VerdictGateBinding,
    allow_legacy_absent: bool,
) -> Result<()> {
    match (
        binding.gate_run_id.as_deref(),
        binding.reused_event_id.as_deref(),
    ) {
        (Some(gate_run_id), None) if !gate_run_id.is_empty() => Ok(()),
        (None, Some(reused_event_id)) if !reused_event_id.is_empty() => Ok(()),
        (None, None) if allow_legacy_absent => Ok(()),
        _ => bail!("verdict gate binding execution reference 非 exact xor tuple"),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_existing_verdict_gates(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    active: &crate::plan::ReadonlyIrValidation,
    ir_task: &crate::plan::IrTask,
    committed_binding: &binding::Binding,
    events: &[EventRecord],
    verdict_position: usize,
    payload: &RootVerdictPayload,
) -> Result<()> {
    if payload.verdict != "PASS" && payload.gates.is_empty() {
        return Ok(());
    }
    if payload.gates.is_empty() {
        bail!("既有 root PASS gate 集合为空");
    }
    let reuse_count = payload
        .gates
        .iter()
        .filter(|gate| gate.reused_event_id.is_some())
        .count();
    let dispatch_position = events[..verdict_position]
        .iter()
        .rposition(|event| {
            event.kind == "DispatchIssued"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
        })
        .context("root verdict 前缺 current DispatchIssued")?;
    let reuse_active = root_gate_reuse_policy_active(root, round, events, task_id, attempt_id)?;
    let allow_legacy_absent = !reuse_active
        && !modern_root_execution_observed(
            &events[dispatch_position + 1..verdict_position],
            round,
            task_id,
            attempt_id,
        );
    for binding in &payload.gates {
        validate_verdict_gate_execution_reference(binding, allow_legacy_absent)?;
    }
    if reuse_count > 0 {
        if reuse_count != payload.gates.len()
            || payload.gates.iter().any(|gate| {
                gate.gate_run_id.is_some()
                    || gate.reused_event_id.as_deref().is_none_or(str::is_empty)
            })
        {
            bail!("root verdict gate bindings 混合 execution references");
        }
        if !reuse_active {
            bail!("GateReused verdict 的 attempt-base policy 未激活");
        }
        let prepared = match plan_live_root_gate_reuse_v1(
            root,
            round,
            task_id,
            attempt_id,
            &payload.head_sha,
            payload.attempt_no,
            active,
            events,
        )? {
            LiveRootGateReusePlanV1::Reuse(prepared) => prepared,
            LiveRootGateReusePlanV1::Execute(miss) => {
                bail!(
                    "existing GateReused verdict no longer validates: {}",
                    miss.detail
                )
            }
        };
        if payload.gates.len() != prepared.gates.len() {
            bail!("GateReused verdict gate count 与 validated collect bundle 不一致");
        }
        let mut positions = Vec::with_capacity(payload.gates.len());
        for (binding, gate) in payload.gates.iter().zip(&prepared.gates) {
            let event_id = binding
                .reused_event_id
                .as_deref()
                .context("GateReused binding 缺 reusedEventId")?;
            let matching = events[..verdict_position]
                .iter()
                .enumerate()
                .filter(|(_, event)| event.event_id == event_id)
                .collect::<Vec<_>>();
            let [(position, event)] = matching.as_slice() else {
                bail!("reusedEventId 在 VerdictIssued 前不唯一");
            };
            validate_reused_gate_event_and_source(
                root,
                events,
                round,
                task_id,
                attempt_id,
                *position,
                event,
                binding,
                gate,
                &prepared.subject,
                &prepared.input_identity_sha256,
                &payload.collect_completed_event_id,
                None,
            )?;
            positions.push(*position);
        }
        if positions.windows(2).any(|pair| pair[0] + 1 != pair[1]) {
            bail!("GateReused events 未保持 verdict gate 顺序的连续 batch block");
        }
        let substitutions = events[..verdict_position]
            .iter()
            .filter(|event| {
                event.kind == "ReviewSeatSubstituted"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round)
                    && nongate_event_payload_str(event, "attemptId") == Some(attempt_id)
            })
            .count();
        let expected_start = verdict_position
            .checked_sub(substitutions + positions.len())
            .context("GateReused same-batch position 下溢")?;
        if positions.first().copied() != Some(expected_start) {
            bail!("GateReused 与 VerdictIssued 未形成同一 checked batch suffix");
        }
        return Ok(());
    }

    if payload
        .gates
        .iter()
        .any(|gate| gate.gate_run_id.is_some() && gate.reused_event_id.is_some())
    {
        bail!("verdict gate binding 同时含 gateRunId/reusedEventId");
    }
    let available = committed_binding
        .commands
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let names = collect::resolved_merge_gate_refs_for_signed_fast(
        committed_binding,
        &ir_task.gates_fast,
        &available,
    )?;
    if names.is_empty() || payload.gates.len() != names.len() {
        bail!("既有 root verdict gates 与 signed merge lane 不一致");
    }
    for (name, bound) in names.iter().zip(&payload.gates) {
        if bound.name != *name || bound.reused_event_id.is_some() {
            bail!("既有 root verdict gate 顺序/名称/reference 不一致");
        }
        if payload.verdict == "PASS" && bound.exit_code != 0 {
            bail!("既有 root PASS 含红 gate");
        }
        if let Some(gate_run_id) = bound.gate_run_id.as_deref() {
            bound_phase_scoped_root_gate_log_bytes(
                root,
                round,
                task_id,
                attempt_id,
                gate_run_id,
                name,
                &bound.log_sha256,
                bound.log_bytes,
            )?;
        } else {
            bound_root_verdict_gate_log_bytes(
                root,
                round,
                task_id,
                attempt_id,
                name,
                &bound.log_sha256,
                bound.log_bytes,
            )?;
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
    let names = collect::resolved_merge_gate_refs_for_signed_fast(
        committed_binding,
        &ir_task.gates_fast,
        &available,
    )?;
    if names.is_empty() {
        bail!("root verdict gate 集合为空");
    }
    let wt = root.join(".worktrees").join(task_id);
    let log_dir = root.join("coordination/runtime/logs");
    let subject_tree_sha = gate::capture_gate_subject_tree(root, &wt)?;
    let mut results = Vec::with_capacity(names.len());
    let mut bindings = Vec::with_capacity(names.len());
    for name in names {
        let spec = committed_binding
            .commands
            .get(&name)
            .with_context(|| format!("绑定缺命令: {name}"))?;
        let gate_run_id = ulid::Ulid::new().to_string();
        let scoped_tag = gate::phase_scoped_log_tag(
            round,
            task_id,
            attempt_id,
            gate::GatePhase::Root,
            &gate_run_id,
        );
        let fingerprint = gate::capture_gate_environment_fingerprint(root)?;
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
            &scoped_tag,
        )?;
        gate::record_gate_execution(
            root,
            round,
            ledger::GateAuditIdentity::Attempt {
                task_id,
                attempt_id,
            },
            GATE_EXECUTED_SCHEMA,
            gate::GatePhase::Root,
            &gate_run_id,
            &subject_tree_sha,
            &result,
            &fingerprint,
        )?;
        let bytes = fs::read(&result.log_path)
            .with_context(|| format!("读取 verdict gate log 失败: {}", result.log_path))?;
        let (sha256, len) = sha_binding(&bytes);
        bindings.push(VerdictGateBinding {
            name: name.clone(),
            exit_code: result.exit_code,
            log_sha256: sha256,
            log_bytes: len,
            gate_run_id: Some(gate_run_id),
            reused_event_id: None,
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

fn append_root_verdict_checked_batch<F>(root: &Path, round: &str, decide: F) -> Result<usize>
where
    F: FnOnce(&[EventRecord]) -> Result<Vec<EventRecord>>,
{
    ledger::append_checked(root, round, decide)
}

fn require_current_open_schema3_verdict_generation(
    root: &Path,
    expected_round: Option<&str>,
) -> Result<String> {
    let round = crate::current_round(root)?;
    if round.is_empty() {
        bail!("orch verdict CURRENT-ROUND 为空");
    }
    if expected_round.is_some_and(|expected| expected != round) {
        bail!("orch verdict 写锁内 current round generation 漂移");
    }
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let read = read_ledger(&ledger_path).with_context(|| {
        format!(
            "读取 orch verdict generation ledger 失败: {}",
            ledger_path.display()
        )
    })?;
    if !read.bad_lines.is_empty() {
        bail!("orch verdict generation guard 拒绝坏账本");
    }
    if crate::round::contract_schema_from_events(&read.events, &round)?
        != Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
    {
        bail!("schema 1/2 root verdict writer 已退役；历史轮仅支持只读 replay");
    }
    if orch_core::fold(&read.events).round_closed {
        bail!("round {round} 已关闭，拒绝 root verdict 写入");
    }
    Ok(round)
}

/// Issue or replay one root verdict for the current open schema-3 generation.
///
/// Historical schema-1/2 rounds are immutable and can only enter the archived
/// validation path; they are rejected before the protocol transition lock.
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
    run_root_verdict_with_quarantine(root, task_id, attempt_id, expected_head,
        expected_main, verdict, reason, dry_run, &[])
}

/// Record root BLOCKED with explicit refusals for exact accepted reviews whose
/// adjudication must be abandoned. Quarantines and BLOCKED form one checked batch;
/// native termination, leases, answers and cleanup authority remain unchanged.
/// Dry-run writes no business facts, and replay requires the original selection.
pub fn run_root_verdict_with_quarantine(
    root: &Path,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
    expected_main: &str,
    verdict: RootVerdict,
    reason: Option<&str>,
    dry_run: bool,
    quarantine_review: &[String],
) -> Result<RootVerdictOutcome> {
    if !quarantine_review.is_empty() {
        if verdict != RootVerdict::Blocked || reason.is_none_or(|s| s.trim().is_empty()) {
            bail!("--quarantine-review requires BLOCKED and a reason");
        }
        let unique = quarantine_review.iter().collect::<BTreeSet<_>>();
        if unique.len() != quarantine_review.len()
            || quarantine_review.iter().any(|s| s.is_empty()
                || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        { bail!("--quarantine-review requires canonical unique wake ids"); }
    }
    let round = require_current_open_schema3_verdict_generation(root, None)?;
    crate::close::with_protocol_transition_named(root, "orch verdict", Some(task_id), || {
        let locked_round = require_current_open_schema3_verdict_generation(root, Some(&round))?;
        run_root_verdict_locked(
            root,
            &locked_round,
            task_id,
            attempt_id,
            expected_head,
            expected_main,
            verdict,
            reason,
            dry_run,
            quarantine_review,
        )
    })
}

fn run_root_verdict_locked(
    root: &Path,
    round: &str,
    task_id: &str,
    attempt_id: &str,
    expected_head: &str,
    expected_main: &str,
    verdict: RootVerdict,
    reason: Option<&str>,
    dry_run: bool,
    quarantine_review: &[String],
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
    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let lr = read_ledger(&ledger_path)?;
    if !lr.bad_lines.is_empty() {
        bail!("root verdict 拒绝坏账本");
    }
    ensure_single_pending_root_pass(&lr.events, round, task_id, attempt_id, verdict)?;
    let active = crate::plan::require_active_round_ir(root, round, &lr.events)?;
    if active.candidate.schema_version != crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        bail!("orch verdict active ROUND-IR 非 schema 3 generation");
    }
    if !is_root_manual_ir(&active.candidate) {
        bail!("orch verdict 仅适用于 root-manual-fixed-head");
    }
    validate_expected_main_contract(
        root,
        round,
        task_id,
        expected_main,
        &active,
        root_verdict_ledger_mode(&lr.events, round, task_id),
    )?;
    let ir_task = active
        .candidate
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .with_context(|| format!("ROUND-IR 不含 task {task_id}"))?;
    validate_repo_tuple(root, task_id, expected_head, expected_main)?;
    let lineage =
        current_attempt_and_collect(&lr.events, task_id, round, attempt_id, expected_head)?;
    validate_current_implementer(&active.candidate, &lineage.implementer_agent)?;
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
        reviews: Vec::new(),
        evidence: Vec::new(),
        gates: Vec::new(),
    };
    let quarantines = crate::generic_review::prepare_review_quarantines(
        &lr.events, round, task_id, attempt_id, reason.unwrap_or("").trim(), quarantine_review,
    )?;
    let prospective_root = ledger::event("VerdictIssued", "verifier:root", Some(task_id),
        Some(round), serde_json::to_value(&payload)?);
    let mut prospective = lr.events.clone();
    if !quarantines.is_empty() {
        prospective.extend(quarantines.iter().cloned());
        prospective.push(prospective_root.clone());
        for quarantine in &quarantines {
            crate::generic_review::canonical_review_quarantine(quarantine, &prospective, round)?;
        }
    }
    let (reviews, evidence, substitutions) = required_file_bindings(
        root, &prospective, round, task_id, attempt_id, expected_head, expected_main,
        &lineage.implementer_agent, ir_task, verdict,
        !quarantines.is_empty(),
    )?;
    payload.reviews = reviews;
    payload.evidence = evidence;

    let existing = matching_existing_root_verdict(&lr.events, &round, task_id, &payload)?;
    let root_position = existing
        .as_ref()
        .map(|(position, _)| *position)
        .unwrap_or(lr.events.len());
    validate_root_authorization_order(
        root,
        &lr.events,
        &round,
        task_id,
        payload.ir_revision,
        &payload.validation_digest,
        attempt_id,
        payload.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        root_position,
    )?;
    if let Some((position, existing)) = existing {
        let signed_binding = committed_attempt_policy_binding(
            root,
            &lr.events,
            round,
            task_id,
            attempt_id,
            expected_main,
        )?;
        validate_review_substitution_events(
            &lr.events,
            round,
            task_id,
            attempt_id,
            position,
            &substitutions,
            Some(&lr.events[position].event_id),
        )?;
        validate_existing_verdict_gates(
            root,
            round,
            task_id,
            attempt_id,
            &active,
            ir_task,
            &signed_binding,
            &lr.events,
            position,
            &existing,
        )?;
        confirm_replayed_verdict_durable(&ledger_path, round, task_id, &payload)?;
        return Ok(RootVerdictOutcome {
            appended: false,
            dry_run,
            verdict: payload.verdict,
            gates: Vec::new(),
        });
    }
    if dry_run {
        return Ok(RootVerdictOutcome {
            appended: false,
            dry_run: true,
            verdict: payload.verdict,
            gates: Vec::new(),
        });
    }

    let signed_binding = committed_attempt_policy_binding(
        root,
        &lr.events,
        &round,
        task_id,
        attempt_id,
        expected_main,
    )?;

    let before_main = crate::gitx::rev_parse(root, "main")?;
    let before_task = crate::gitx::rev_parse(root, &format!("refs/heads/task/{task_id}"))?;
    let reuse_active =
        root_gate_reuse_policy_active(root, &round, &lr.events, task_id, attempt_id)?;
    let mut prepared_reuse = None;
    let mut reused_events = Vec::new();
    let (gate_results, gate_bindings) = if verdict != RootVerdict::Pass && reuse_active {
        (Vec::new(), Vec::new())
    } else if verdict == RootVerdict::Pass && reuse_active {
        match plan_live_root_gate_reuse_v1(
            root,
            &round,
            task_id,
            attempt_id,
            expected_head,
            lineage.attempt_no,
            &active,
            &lr.events,
        )? {
            LiveRootGateReusePlanV1::Reuse(prepared) => {
                reused_events = build_root_gate_reused_events(
                    &lr.events, &round, task_id, attempt_id, &prepared,
                )?;
                let outputs = root_gate_reused_outputs(root, &prepared, &reused_events)?;
                prepared_reuse = Some(prepared);
                outputs
            }
            LiveRootGateReusePlanV1::Execute(miss) => {
                append_root_gate_reuse_miss_once(
                    root,
                    &round,
                    task_id,
                    attempt_id,
                    lineage.attempt_no,
                    &miss,
                )?;
                run_verdict_gates(root, &round, task_id, attempt_id, ir_task, &signed_binding)?
            }
        }
    } else {
        run_verdict_gates(root, &round, task_id, attempt_id, ir_task, &signed_binding)?
    };
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

    let payload_value = serde_json::to_value(&payload)?;
    let appended = append_root_verdict_checked_batch(root, &round, |events| {
        if crate::current_round(root)? != round
            || crate::round::contract_schema_from_events(events, round)?
                != Some(crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION)
            || orch_core::fold(events).round_closed
        {
            bail!("verdict append 锁内 current/open schema 3 generation 漂移");
        }
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
        let fresh_quarantines = crate::generic_review::prepare_review_quarantines(
            events, &round, task_id, attempt_id, reason.unwrap_or("").trim(), quarantine_review,
        )?;
        let same_quarantines = fresh_quarantines.len() == quarantines.len()
            && fresh_quarantines.iter().zip(&quarantines).all(|(fresh, prepared)| {
                fresh.kind == prepared.kind && fresh.actor == prepared.actor
                    && fresh.round == prepared.round && fresh.task_id == prepared.task_id
                    && fresh.payload == prepared.payload
            });
        if !same_quarantines { bail!("review quarantine plan changed before append"); }
        let mut fresh_prospective = events.to_vec();
        if !quarantines.is_empty() {
            fresh_prospective.extend(quarantines.iter().cloned());
            let mut root_event = prospective_root.clone();
            root_event.payload = Some(payload_value.clone());
            fresh_prospective.push(root_event);
            for quarantine in &quarantines {
                crate::generic_review::canonical_review_quarantine(quarantine, &fresh_prospective, round)?;
            }
        }
        let (fresh_reviews, fresh_evidence, fresh_substitutions) = required_file_bindings(
            root,
            &fresh_prospective,
            &round,
            task_id,
            attempt_id,
            expected_head,
            expected_main,
            &fresh_lineage.implementer_agent,
            fresh_task,
            verdict,
            !quarantines.is_empty(),
        )?;
        if fresh_reviews != payload.reviews
            || fresh_evidence != payload.evidence
            || fresh_substitutions != substitutions
        {
            bail!("verdict append 前 review/evidence bytes 漂移");
        }
        let existing = matching_existing_root_verdict(events, &round, task_id, &payload)?;
        let root_position = existing
            .as_ref()
            .map(|(position, _)| *position)
            .unwrap_or(events.len());
        validate_root_authorization_order(
            root,
            events,
            &round,
            task_id,
            payload.ir_revision,
            &payload.validation_digest,
            attempt_id,
            payload.bootstrap_pre_signoff_attempt.as_deref(),
            &fresh_lineage,
            root_position,
        )?;
        if let Some(expected_reuse) = prepared_reuse.as_ref() {
            if !root_gate_reuse_policy_active(root, &round, events, task_id, attempt_id)? {
                bail!("verdict append 前 root-reuse-v1 policy 漂移");
            }
            let fresh_reuse = match plan_live_root_gate_reuse_v1(
                root,
                &round,
                task_id,
                attempt_id,
                expected_head,
                payload.attempt_no,
                &fresh_active,
                events,
            )? {
                LiveRootGateReusePlanV1::Reuse(prepared) => prepared,
                LiveRootGateReusePlanV1::Execute(miss) => {
                    bail!("verdict append 前 collect reuse 漂移: {}", miss.detail)
                }
            };
            if &fresh_reuse != expected_reuse
                || reused_events.len() != expected_reuse.gates.len()
                || reused_events.iter().any(|event| {
                    events
                        .iter()
                        .any(|existing| existing.event_id == event.event_id)
                })
            {
                bail!("verdict append 前 GateReused plan/event identity 漂移");
            }
        } else if !reused_events.is_empty() {
            bail!("verdict append 收到无 planned reuse 的 GateReused events");
        }
        if let Some((position, existing)) = existing {
            validate_review_substitution_events(
                events,
                &round,
                task_id,
                attempt_id,
                position,
                &substitutions,
                Some(&events[position].event_id),
            )?;
            validate_existing_verdict_gates(
                root,
                &round,
                task_id,
                attempt_id,
                &fresh_active,
                fresh_task,
                &signed_binding,
                events,
                position,
                &existing,
            )?;
            return Ok(Vec::new());
        }
        let verdict_event = ledger::event("VerdictIssued", "verifier:root", Some(task_id),
            Some(&round), payload_value.clone());
        let mut batch = reused_events.clone();
        batch.extend(substitutions.iter().map(|substitution| {
                ledger::event(
                    "ReviewSeatSubstituted",
                    "runtime:orch",
                    Some(task_id),
                    Some(&round),
                    serde_json::json!({
                        "attemptId": attempt_id,
                        "role": substitution.role,
                        "fromAgent": substitution.from_agent,
                        "toAgent": substitution.to_agent,
                        "reviewedHead": substitution.reviewed_head,
                        "sourceTerminalEventId": substitution.source_terminal_event_id,
                        "nongateDeliveryEventId": substitution.nongate_delivery_event_id,
                        "verdictEventId": verdict_event.event_id,
                    }),
                )
            }));
        batch.extend(quarantines.iter().cloned());
        batch.push(verdict_event);
        Ok(batch)
    })?;
    if appended == 0 {
        // H21 机械侧根治：幂等重放（decide 在锁内 fresh-read 里找到既有
        // verdict 而追加 0 条）绝不能只凭内存快照报成功——回读账本确认该
        // attempt 的 canonical VerdictIssued 真实在场，否则 Err。
        confirm_replayed_verdict_durable(&ledger_path, &round, task_id, &payload)?;
    }
    Ok(RootVerdictOutcome {
        appended: appended > 0,
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
    if !is_root_manual_ir(&active.candidate) {
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
        root,
        events,
        round,
        task_id,
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
    let (reviews, evidence, substitutions) = required_file_bindings(
        root,
        events,
        round,
        task_id,
        &payload.attempt_id,
        &payload.head_sha,
        &payload.main_head_sha,
        &lineage.implementer_agent,
        ir_task,
        RootVerdict::Pass,
        false,
    )?;
    if reviews != payload.reviews || evidence != payload.evidence {
        bail!("root PASS 后 review/evidence bytes 已改变");
    }
    validate_review_substitution_events(
        events,
        round,
        task_id,
        &payload.attempt_id,
        root_position,
        &substitutions,
        Some(&event.event_id),
    )?;
    let signed_binding = committed_attempt_policy_binding(
        root,
        events,
        round,
        task_id,
        &payload.attempt_id,
        &payload.main_head_sha,
    )?;
    validate_existing_verdict_gates(
        root,
        round,
        task_id,
        &payload.attempt_id,
        &active,
        ir_task,
        &signed_binding,
        events,
        root_position,
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
/// An unrecorded chain remains bound to the current active IR.  Once current
/// main already commits the task's unique `TaskRecorded`, complete-seal replay
/// instead revalidates that historical IR/root/merge chain and permits only an
/// exact managed-terminal cleanup tail beyond the current-main ledger blob.
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

fn completed_record_replay_authorization(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
) -> Result<Option<RootRecordAuthorization>> {
    let current_main = crate::gitx::rev_parse(root, "main")?;
    let ledger_rel = format!("coordination/rounds/{round}/events.jsonl");
    let committed_bytes = committed_regular_blob_bytes(
        root,
        &current_main,
        &ledger_rel,
        "completed record replay current-main ledger",
    )?;
    let committed = parse_strict_committed_ledger(
        &committed_bytes,
        "completed record replay current-main ledger",
    )?;
    let recorded_positions = committed
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "TaskRecorded"
                && event.round.as_deref() == Some(round)
                && event.task_id.as_deref() == Some(task_id)
        })
        .map(|(position, _)| position)
        .collect::<Vec<_>>();
    if recorded_positions.is_empty() {
        return Ok(None);
    }
    if crate::gitx::current_branch(root)?.as_deref() != Some("main")
        || crate::gitx::rev_parse(root, "HEAD")? != current_main
    {
        bail!("completed record replay 要求主工作区检出 main 且 HEAD==main");
    }
    let working_bytes = regular_file_bytes(
        root,
        &root.join(&ledger_rel),
        "completed record replay working ledger",
    )?;
    if !working_bytes.starts_with(&committed_bytes) {
        bail!("completed record replay working ledger 不是 current-main ledger 的逐字节前缀扩展");
    }
    let suffix_bytes = &working_bytes[committed_bytes.len()..];
    let suffix = if suffix_bytes.is_empty() {
        Vec::new()
    } else {
        parse_strict_committed_ledger(
            suffix_bytes,
            "completed record replay working ledger suffix",
        )?
    };
    if committed.len() + suffix.len() != events.len()
        || !event_values_equal(&committed, &events[..committed.len()])?
        || !event_values_equal(&suffix, &events[committed.len()..])?
    {
        bail!("completed record replay parsed ledger 未逐事件绑定 working bytes");
    }

    let active = crate::plan::require_active_round_ir(root, round, events)
        .context("completed record replay 要求 current signed active IR")?;
    validate_archived_record_chain(root, round, task_id, &committed)?;
    let [recorded_position] = recorded_positions.as_slice() else {
        bail!("completed record replay 要求 current main 恰好一条 target TaskRecorded");
    };
    if !suffix.is_empty()
        && !canonical_complete_seal_managed_terminal_suffixes_v1(
            &committed,
            &suffix,
            round,
            task_id,
        )?
    {
        bail!("completed record replay working suffix 非 canonical managed terminal batch");
    }
    if active.candidate.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        let merges = committed
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.kind == "MergeExecuted"
                    && event.task_id.as_deref() == Some(task_id)
                    && event.round.as_deref() == Some(round)
            })
            .collect::<Vec<_>>();
        let [(merge_position, merge_event)] = merges.as_slice() else {
            bail!("completed schema 3 replay 要求唯一 MergeExecuted");
        };
        let merged: MergeExecutedPayload = serde_json::from_value(
            merge_event
                .payload
                .clone()
                .context("completed schema 3 MergeExecuted 缺 payload")?,
        )?;
        let (_, started) = final_recorded_merge_start(
            &committed,
            round,
            task_id,
            *merge_position,
            *recorded_position,
        )?;
        let roots = committed
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event_id == started.verdict_event_id)
            .collect::<Vec<_>>();
        let [(root_position, root_event)] = roots.as_slice() else {
            bail!("completed schema 3 replay 缺唯一 referenced root verdict");
        };
        let root_payload = parse_root_payload(root_event)?;
        let signoffs = crate::plan::matching_user_plan_signoff_positions(
            &committed[..*root_position],
            round,
            root_payload.ir_revision,
            &root_payload.validation_digest,
        )?;
        let [signoff_position] = signoffs.as_slice() else {
            bail!("completed schema 3 replay 缺唯一 PlanSignedOff");
        };
        let ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
        let ir_bytes = committed_regular_blob_bytes(
            root,
            &root_payload.main_head_sha,
            &ir_rel,
            "completed schema 3 signed ROUND-IR",
        )?;
        let historical_ir = parse_historical_round_ir_bytes(
            &ir_bytes,
            "completed schema 3 signed ROUND-IR 非 canonical",
        )?;
        let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
        let task_card_sha256 = historical_ir
            .source_bindings
            .task_cards
            .get(&card_rel)
            .context("completed schema 3 IR 未绑定 task card")?
            .clone();
        return Ok(Some(RootRecordAuthorization {
            verdict_event_id: root_event.event_id.clone(),
            plan_signed_off_event_id: committed[*signoff_position].event_id.clone(),
            attempt_id: root_payload.attempt_id,
            attempt_no: root_payload.attempt_no,
            implementer_agent: root_payload.implementer_agent,
            head_sha: root_payload.head_sha,
            expected_main_sha: root_payload.main_head_sha,
            merge_sha: merged.merge_sha,
            ir_revision: root_payload.ir_revision,
            validation_digest: root_payload.validation_digest,
            task_card_sha256,
            reviews: root_payload.reviews,
            already_recorded: true,
        }));
    }
    let frozen =
        recorded_frozen_authorization(root, round, task_id, &committed, *recorded_position)?;
    let RecordedFrozenAuthorization {
        root_payload,
        verdict_event_id,
        plan_signed_off_event_id,
        task_card_sha256,
        merge_sha,
        ..
    } = frozen;
    Ok(Some(RootRecordAuthorization {
        verdict_event_id,
        plan_signed_off_event_id,
        attempt_id: root_payload.attempt_id,
        attempt_no: root_payload.attempt_no,
        implementer_agent: root_payload.implementer_agent,
        head_sha: root_payload.head_sha,
        expected_main_sha: root_payload.main_head_sha,
        merge_sha,
        ir_revision: root_payload.ir_revision,
        validation_digest: root_payload.validation_digest,
        task_card_sha256,
        reviews: root_payload.reviews,
        already_recorded: true,
    }))
}

fn validate_root_record_authorization_inner(
    root: &Path,
    round: &str,
    task_id: &str,
    events: &[EventRecord],
    mode: RecordAuthorizationMode,
) -> Result<RootRecordAuthorization> {
    if mode == RecordAuthorizationMode::MergeExecuted {
        if let Some(authorization) =
            completed_record_replay_authorization(root, round, task_id, events)?
        {
            return Ok(authorization);
        }
    }
    let active = crate::plan::require_active_round_ir(root, round, events)?;
    if !is_root_manual_ir(&active.candidate) {
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
        root,
        events,
        round,
        task_id,
        payload.ir_revision,
        &payload.validation_digest,
        &payload.attempt_id,
        payload.bootstrap_pre_signoff_attempt.as_deref(),
        &lineage,
        root_position,
    )?;
    let signoff_positions = crate::plan::matching_user_plan_signoff_positions(
        &events[..root_position],
        round,
        payload.ir_revision,
        &payload.validation_digest,
    )?;
    if signoff_positions.len() != 1 {
        bail!("record authorization 要求唯一 exact PlanSignedOff eventId");
    }
    let plan_signed_off_event_id = events[signoff_positions[0]].event_id.clone();
    let task_card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let task_card_sha256 = active
        .candidate
        .source_bindings
        .task_cards
        .get(&task_card_rel)
        .with_context(|| {
            format!("record authorization active IR 未绑定 task card: {task_card_rel}")
        })?
        .clone();
    validate_current_implementer(&active.candidate, &lineage.implementer_agent)?;
    if lineage.attempt_no != payload.attempt_no
        || lineage.implementer_agent != payload.implementer_agent
        || lineage.collect.event_id != payload.collect_completed_event_id
    {
        bail!("record root PASS 与 current attempt/latest collect 不一致");
    }
    let (reviews, evidence, substitutions) = required_file_bindings(
        root,
        events,
        round,
        task_id,
        &payload.attempt_id,
        &payload.head_sha,
        &payload.main_head_sha,
        &lineage.implementer_agent,
        ir_task,
        RootVerdict::Pass,
        false,
    )?;
    if reviews != payload.reviews || evidence != payload.evidence {
        bail!("record 前 review/evidence bytes 已改变");
    }
    validate_review_substitution_events(
        events,
        round,
        task_id,
        &payload.attempt_id,
        root_position,
        &substitutions,
        Some(&root_event.event_id),
    )?;
    let signed_binding = committed_attempt_policy_binding(
        root,
        events,
        round,
        task_id,
        &payload.attempt_id,
        &payload.main_head_sha,
    )?;
    validate_existing_verdict_gates(
        root,
        round,
        task_id,
        &payload.attempt_id,
        &active,
        ir_task,
        &signed_binding,
        events,
        root_position,
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
        plan_signed_off_event_id,
        attempt_id: payload.attempt_id,
        attempt_no: payload.attempt_no,
        implementer_agent: lineage.implementer_agent,
        head_sha: payload.head_sha,
        expected_main_sha: payload.main_head_sha,
        merge_sha,
        ir_revision: active.persisted_revision,
        validation_digest: active.persisted_digest,
        task_card_sha256,
        reviews: payload.reviews,
        already_recorded: !recorded.is_empty(),
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_archived_root_gate_bindings_v1(
    root: &Path,
    events: &[EventRecord],
    round: &str,
    task_id: &str,
    root_position: usize,
    observation_start: usize,
    historical_ir: &crate::plan::RoundIr,
    historical_task: &crate::plan::IrTask,
    payload: &RootVerdictPayload,
) -> Result<()> {
    let reuse_count = payload
        .gates
        .iter()
        .filter(|gate| gate.reused_event_id.is_some())
        .count();
    let reuse_active =
        root_gate_reuse_policy_active(root, round, events, task_id, &payload.attempt_id)?;
    let observation_suffix = events
        .get(observation_start..root_position)
        .context("archived root observation suffix 越界")?;
    let allow_legacy_absent = !reuse_active
        && !modern_root_execution_observed(observation_suffix, round, task_id, &payload.attempt_id);
    for binding in &payload.gates {
        validate_verdict_gate_execution_reference(binding, allow_legacy_absent)?;
    }
    let expected_subject_tree =
        crate::gitx::rev_parse(root, &format!("{}^{{tree}}", payload.head_sha))?;
    if reuse_count == 0 {
        let committed_binding = committed_attempt_policy_binding(
            root,
            events,
            round,
            task_id,
            &payload.attempt_id,
            &payload.main_head_sha,
        )?;
        let available = committed_binding
            .commands
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let resolved_merge = collect::resolved_merge_gate_refs_for_signed_fast(
            &committed_binding,
            &historical_task.gates_fast,
            &available,
        )?;
        let names = archived_spawned_root_gate_names_v1(
            allow_legacy_absent,
            &historical_task.gates_fast,
            &resolved_merge,
        )?;
        if names.len() != payload.gates.len() {
            bail!("archived spawned root gates 未绑定 signed merge lane");
        }
        let observation_suffix = &events[observation_start..root_position];
        let mut previous = None;
        for (name, binding) in names.iter().zip(&payload.gates) {
            if binding.name != *name || binding.reused_event_id.is_some() {
                bail!("archived spawned root gate name/reference 漂移");
            }
            let Some(gate_run_id) = binding.gate_run_id.as_deref() else {
                continue;
            };
            let matching = observation_suffix
                .iter()
                .enumerate()
                .filter(|(_, event)| {
                    let Some(gate) = canonical_gate_executed_payload(event, round) else {
                        return false;
                    };
                    event.task_id.as_deref() == Some(task_id)
                        && gate.get("phase").and_then(serde_json::Value::as_str) == Some("root")
                        && gate.get("gateRunId").and_then(serde_json::Value::as_str)
                            == Some(gate_run_id)
                        && gate.get("commandRef").and_then(serde_json::Value::as_str)
                            == Some(name.as_str())
                        && gate.get("exitCode").and_then(serde_json::Value::as_i64)
                            == Some(i64::from(binding.exit_code))
                        && gate
                            .get("subjectTreeSha")
                            .and_then(serde_json::Value::as_str)
                            == Some(expected_subject_tree.as_str())
                        && gate.get("logSha256").and_then(serde_json::Value::as_str)
                            == Some(binding.log_sha256.as_str())
                        && gate.get("logBytes").and_then(serde_json::Value::as_u64)
                            == Some(binding.log_bytes)
                })
                .map(|(offset, _)| observation_start + offset)
                .collect::<Vec<_>>();
            let [position] = matching.as_slice() else {
                bail!("archived root gate binding 未精确绑定唯一 GateExecuted observation");
            };
            if previous.is_some_and(|prior| prior >= *position) {
                bail!("archived root GateExecuted 顺序与 verdict binding 不一致");
            }
            previous = Some(*position);
        }
        return Ok(());
    }

    if reuse_count != payload.gates.len()
        || payload.gates.iter().any(|gate| {
            gate.gate_run_id.is_some() || gate.reused_event_id.as_deref().is_none_or(str::is_empty)
        })
    {
        bail!("archived root verdict 混合 execution references");
    }
    if !reuse_active {
        bail!("archived GateReused verdict 的 attempt-base policy 未激活");
    }
    let first_id = payload.gates[0]
        .reused_event_id
        .as_deref()
        .context("archived first reused binding 缺 reusedEventId")?;
    let first_reused = events[..root_position]
        .iter()
        .enumerate()
        .filter(|(_, event)| event.event_id == first_id)
        .collect::<Vec<_>>();
    let [(first_position, first_event)] = first_reused.as_slice() else {
        bail!("archived first reusedEventId 不唯一");
    };
    let Some(ledger::RuntimeEventPayloadV1::GateReused(first_payload)) =
        ledger::decode_runtime_event_v1(first_event)?
    else {
        bail!("archived first reusedEventId 非 GateReused");
    };
    let receipt_matches = events[..*first_position]
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == "CollectGateSuccessReceipt"
                && event.actor == "runtime:orch"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && event
                    .payload
                    .as_ref()
                    .and_then(|value| value.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(payload.attempt_id.as_str())
                && event
                    .payload
                    .as_ref()
                    .and_then(|value| value.get("gates"))
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|gates| {
                        gates.iter().any(|gate| {
                            gate.get("gateEventId").and_then(serde_json::Value::as_str)
                                == Some(first_payload.source_gate_event_id.as_str())
                        })
                    })
        })
        .collect::<Vec<_>>();
    let [(receipt_position, _)] = receipt_matches.as_slice() else {
        bail!("archived GateReused 缺唯一 source collect receipt");
    };
    let attestation = root_reuse_collect_attestation_from_committed_receipt(events, *receipt_position)?;
    let card_rel = format!("coordination/rounds/{round}/tasks/{task_id}.md");
    let historical_card_sha256 = historical_ir
        .source_bindings
        .task_cards
        .get(&card_rel)
        .context("archived ROUND-IR 未绑定 root reuse task card")?;
    let card_bytes = committed_regular_blob_bytes(
        root,
        &payload.main_head_sha,
        &card_rel,
        "archived root reuse card",
    )?;
    let (committed_card_sha256, _) = sha_binding(&card_bytes);
    if committed_card_sha256 != *historical_card_sha256 {
        bail!("archived root reuse committed card bytes 漂移");
    }
    let card_text =
        std::str::from_utf8(&card_bytes).context("archived root reuse committed card 非 UTF-8")?;
    let bound_card = card::parse(&card_rel, task_id, card_text)?;
    let dispatch = crate::attempt::resolve_current_dispatch(events, task_id, round)?;
    let policy_base_sha = dispatch
        .base_sha
        .context("archived root reuse DispatchIssued 缺 baseSha")?;
    let lane_plan = collect::resolve_collect_lane_plan_at_candidate(
        root,
        round,
        &bound_card,
        events,
        &payload.attempt_id,
        &policy_base_sha,
        &payload.head_sha,
    )?;
    let bundle = CollectGateBundleV1 {
        subject_tree_sha: attestation.subject_tree_sha.clone(),
        ir_revision: attestation.ir_revision,
        validation_digest: attestation.validation_digest.clone(),
        binding_sha256: attestation.binding_sha256.clone(),
        task_card_sha256: attestation.task_card_sha256.clone(),
        resolved_command_digest: attestation.resolved_command_digest.clone(),
        toolchain_digest: attestation.toolchain_digest.clone(),
        environment_digest: attestation.environment_digest.clone(),
        gates: attestation
            .gates
            .iter()
            .map(|gate| ReusedGateV1 {
                name: gate.command_ref.clone(),
                exit_code: gate.exit_code,
                source_event_id: gate.gate_event_id.clone(),
                log_sha256: gate.raw_log_cas_sha256.clone(),
                log_bytes: gate.raw_log_len,
            })
            .collect(),
    };
    let subject = RootGateSubjectV1 {
        subject_tree_sha: expected_subject_tree,
        ir_revision: payload.ir_revision,
        validation_digest: payload.validation_digest.clone(),
        binding_sha256: historical_ir.source_bindings.binding_sha256.clone(),
        task_card_sha256: historical_card_sha256.clone(),
        resolved_command_digest: lane_plan.resolved_command_digest().to_string(),
        toolchain_digest: attestation.toolchain_digest.clone(),
        environment_digest: attestation.environment_digest.clone(),
    };
    if attestation.round != round
        || attestation.task_id != task_id
        || attestation.attempt_id != payload.attempt_id
        || attestation.attempt_no != payload.attempt_no as u64
        || attestation.base_sha != policy_base_sha
        || attestation.branch_sha != payload.head_sha
        || attestation.configured_command_refs != lane_plan.ordered_command_refs()
        || attestation.source_reader_descriptor_sha256.as_deref()
            != lane_plan.source_reader_digests().map(|(descriptor, _)| descriptor)
        || attestation.source_reader_base_sha256.as_deref()
            != lane_plan.source_reader_digests().map(|(_, base)| base)
        || !matches!(
            plan_root_gate_reuse_v1(&bundle, &subject),
            RootGateReuseDecisionV1::Reuse(_)
        )
    {
        bail!("archived collect/root reuse identity no longer matches");
    }
    let input_identity_sha256 = root_gate_reuse_json_sha256(
        "root-gate-reuse-input-v1",
        &RootGateReuseInputIdentityV1 {
            contract_version: ROOT_GATE_REUSE_CONTRACT_V1,
            attempt_id: &payload.attempt_id,
            policy_base_sha: &policy_base_sha,
            bundle: &bundle,
            subject: &subject,
        },
    )?;
    if payload.gates.len() != bundle.gates.len() {
        bail!("archived GateReused bindings 与 collect gates 数量不一致");
    }
    let mut positions = Vec::with_capacity(payload.gates.len());
    for (binding, gate) in payload.gates.iter().zip(&bundle.gates) {
        let event_id = binding
            .reused_event_id
            .as_deref()
            .context("archived GateReused binding 缺 reusedEventId")?;
        let matching = events[..root_position]
            .iter()
            .enumerate()
            .filter(|(_, event)| event.event_id == event_id)
            .collect::<Vec<_>>();
        let [(position, event)] = matching.as_slice() else {
            bail!("archived reusedEventId 在 root 前缀不唯一");
        };
        if *position < observation_start {
            bail!("archived GateReused 不在 expected-main observation suffix");
        }
        validate_reused_gate_event_and_source(
            root,
            events,
            round,
            task_id,
            &payload.attempt_id,
            *position,
            event,
            binding,
            gate,
            &subject,
            &input_identity_sha256,
            &payload.collect_completed_event_id,
            Some(&attestation),
        )?;
        positions.push(*position);
    }
    if positions.windows(2).any(|pair| pair[0] + 1 != pair[1]) {
        bail!("archived GateReused events 未保持同批连续顺序");
    }
    let substitutions = events[..root_position]
        .iter()
        .filter(|event| {
            event.kind == "ReviewSeatSubstituted"
                && event.task_id.as_deref() == Some(task_id)
                && event.round.as_deref() == Some(round)
                && nongate_event_payload_str(event, "attemptId")
                    == Some(payload.attempt_id.as_str())
        })
        .count();
    let expected_start = root_position
        .checked_sub(substitutions + positions.len())
        .context("archived GateReused batch position 下溢")?;
    if positions.first().copied() != Some(expected_start) {
        bail!("archived GateReused/VerdictIssued 不属于同一 checked batch suffix");
    }
    Ok(())
}

fn archived_spawned_root_gate_names_v1(
    allow_legacy_absent: bool,
    signed_fast: &[String],
    resolved_merge: &[String],
) -> Result<Vec<String>> {
    let selected = if allow_legacy_absent {
        // Before phase-scoped GateExecuted observations existed, root ran the
        // task's signed fast list. Reinterpreting that immutable history with
        // a later merge-lane resolver manufactures an extra gate. The caller
        // derives this arm only when both execution references and modern root
        // observations are absent; every modern archive remains on merge lane.
        signed_fast
    } else {
        resolved_merge
    };
    if selected.is_empty() {
        bail!("archived spawned root gate lane 为空");
    }
    Ok(selected.to_vec())
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
    crate::ledger::validate_runtime_event_history_v1_at_root(root, events, round)?;
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
    if historical_events.len() > root_position
        || !event_values_equal(&historical_events, &events[..historical_events.len()])?
    {
        bail!("archived root authorization prefix 未逐事件绑定 mainHead committed ledger");
    }
    let observation_suffix = &events[historical_events.len()..root_position];
    // `canonical_gate_storage_audit_event` is documented as the single
    // exact-shape predicate shared by storage append authority and
    // expected-main suffix validation; this call site was the one that did not
    // share it. A gate refused for low disk and then recovered is honest
    // history authored by the same runtime actor, so the archived chain must
    // admit it exactly as the suffix contract already does. Scope stays exact:
    // the predicate itself demands runtime actor, non-empty task/round and the
    // closed payload shape, and the round is re-bound here.
    for event in observation_suffix {
        let runtime_v1 = runtime_event_allowed_in_committed_mode(
            event,
            round,
            CommittedLedgerMode::CanonicalVerdictSuffix,
        )?;
        if canonical_gate_executed_payload(event, round).is_none()
            && event.kind != "ReviewSeatSubstituted"
            && !runtime_v1
            && !crate::ledger::canonical_gate_storage_audit_event(event)
                .is_some_and(|audit| audit.round == round)
        {
            bail!("archived root authorization prefix 含非 canonical runtime observation suffix");
        }
    }
    let historical_ir_rel = format!("coordination/rounds/{round}/ROUND-IR.yaml");
    let historical_ir_bytes = committed_regular_blob_bytes(
        root,
        &started.main_head_sha,
        &historical_ir_rel,
        "archived signed ROUND-IR",
    )?;
    let historical_ir = parse_historical_round_ir_bytes(
        &historical_ir_bytes,
        "解析 archived mainHead ROUND-IR blob 失败",
    )?;
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
    validate_archived_root_gate_bindings_v1(
        root,
        events,
        round,
        task_id,
        root_position,
        historical_events.len(),
        &historical_ir,
        historical_task,
        &root_payload,
    )?;
    if historical_ir.schema_version == crate::plan::ACTORLESS_ROUND_IR_SCHEMA_VERSION {
        crate::oracle::validate_v3_relocation_history(
            events,
            round,
            task_id,
            root_payload.ir_revision,
            &root_payload.validation_digest,
            true,
            historical_task.has_seeds,
        )?;
        validate_current_implementer(&historical_ir, &root_payload.implementer_agent)?;
        let (reviews, evidence, substitutions) = required_file_bindings(
            root,
            events,
            round,
            task_id,
            &root_payload.attempt_id,
            &root_payload.head_sha,
            &root_payload.main_head_sha,
            &root_payload.implementer_agent,
            historical_task,
            RootVerdict::Pass,
            false,
        )?;
        if !substitutions.is_empty()
            || reviews != root_payload.reviews
            || evidence != root_payload.evidence
        {
            bail!("archived schema 3 review/evidence facts 未精确重放");
        }
        let lineage = archived_attempt_and_collect(
            events,
            task_id,
            round,
            &root_payload.attempt_id,
            &root_payload.head_sha,
        )?;
        validate_root_authorization_order(
            root,
            events,
            round,
            task_id,
            root_payload.ir_revision,
            &root_payload.validation_digest,
            &root_payload.attempt_id,
            None,
            &lineage,
            root_position,
        )?;
        if lineage.attempt_no != root_payload.attempt_no
            || lineage.implementer_agent != "local"
            || lineage.collect.event_id != root_payload.collect_completed_event_id
        {
            bail!("archived schema 3 root PASS 未绑定 local dispatch/collect lineage");
        }
        let current_main = crate::gitx::rev_parse(root, "main")?;
        if !crate::gitx::is_ancestor(root, &merged.merge_sha, &current_main)? {
            bail!("archived schema 3 merge SHA 不是 current main ancestor");
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
        return Ok(());
    }
    let review_mode = review_contract_mode_for_attempt(
        root,
        round,
        events,
        task_id,
        &root_payload.attempt_id,
        historical_task,
    )?;
    if review_mode == ReviewContractMode::Quorum {
        validate_archived_review_transition_chain(
            events,
            round,
            task_id,
            root_position,
            &root_payload,
            historical_task,
        )?;
    }
    let archived_substitutions = match review_mode {
        ReviewContractMode::Panel => {
            let (reviews, evidence, substitutions) = panel_file_bindings(
                root,
                events,
                round,
                task_id,
                &root_payload.attempt_id,
                &root_payload.head_sha,
                &root_payload.main_head_sha,
                &root_payload.implementer_agent,
                historical_task,
                RootVerdict::Pass,
            )?;
            if reviews != root_payload.reviews || evidence != root_payload.evidence {
                bail!("archived panel root bindings 未精确重放");
            }
            substitutions
        }
        ReviewContractMode::Quorum => validate_archived_quorum_binding_membership(
            &root_payload.reviews,
            &root_payload.evidence,
            historical_task,
            &root_payload.head_sha,
        )?,
        ReviewContractMode::LegacyStrict => {
            validate_archived_required_binding_membership(
                &root_payload.reviews,
                &root_payload.evidence,
                &historical_task.required_reviews,
                &historical_task.required_evidence,
            )?;
            Vec::new()
        }
    };
    enforce_signed_smartclaw_fixed_primary_pass_v1(
        root,
        events,
        round,
        task_id,
        &root_payload.attempt_id,
        &root_payload.head_sha,
        historical_task,
        RootVerdict::Pass,
        &root_payload.reviews,
    )?;
    validate_review_substitution_events(
        events,
        round,
        task_id,
        &root_payload.attempt_id,
        root_position,
        &archived_substitutions,
        Some(&root_event.event_id),
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
        if (review_mode != ReviewContractMode::Panel && review.path != canonical_path)
            || !safe_identity_component(&review.role)
            || !safe_identity_component(&review.reviewer)
            || review.verdict != "PASS"
            || review.reviewer == root_payload.implementer_agent
            || !review_paths.insert(review.path.as_str())
            || !reviewers.insert(review.reviewer.as_str())
        {
            bail!("archived root PASS review binding 非 canonical");
        }
        if review_mode == ReviewContractMode::LegacyStrict
            && !review_roles.insert(review.role.as_str())
        {
            bail!("archived legacy root PASS review role 重复");
        }
        full_sha256(&review.sha256, "archived root PASS review.sha256")?;
    }
    if review_mode == ReviewContractMode::LegacyStrict && primary_count != 1 {
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
    let mut execution_refs = std::collections::BTreeSet::new();
    for (index, gate) in root_payload.gates.iter().enumerate() {
        if gate.name.is_empty() || (gate.reused_event_id.is_some() && gate.log_bytes == 0) {
            bail!("archived root PASS gate binding 缺 name/reuse log bytes");
        }
        let reference = match (&gate.gate_run_id, &gate.reused_event_id) {
            (Some(run_id), None) => format!("run:{run_id}"),
            (None, Some(event_id)) => format!("reuse:{event_id}"),
            (None, None) => format!("legacy:{index}:{}", gate.name),
            (Some(_), Some(_)) => bail!("archived gate binding execution reference 非 xor"),
        };
        if !execution_refs.insert(reference) {
            bail!("archived root PASS gate execution reference 重复");
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
        root,
        events,
        round,
        task_id,
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
    if review_mode == ReviewContractMode::Quorum {
        for review in &root_payload.reviews {
            let bytes = match current_main_review_blob(root, &current_main, &review.path)? {
                CurrentMainReviewBlob::Regular(bytes) => bytes,
                CurrentMainReviewBlob::Missing => {
                    bail!(
                        "archived quorum review artifact 后来从 current main 删除: {}",
                        review.path
                    )
                }
                CurrentMainReviewBlob::NonRegular(mode) => bail!(
                    "archived quorum review artifact 后来变成非 regular blob: {} mode={mode}",
                    review.path
                ),
            };
            let (sha256, byte_len) = sha_binding(&bytes);
            if sha256 != review.sha256 || byte_len != review.bytes {
                bail!(
                    "archived quorum review artifact current-main bytes 漂移: {}",
                    review.path
                );
            }
            let expectation = ReviewContractExpectation::exact(
                task_id,
                round,
                &root_payload.attempt_id,
                &review.role,
                &review.reviewer,
                &root_payload.head_sha,
            )?;
            let checked = check_review_artifact_contract(&bytes, &expectation)?
                .context("archived quorum review artifact incomplete")?;
            if checked.substantive_body_len() == 0 || checked.verdict() != review.verdict {
                bail!(
                    "archived quorum review artifact body/verdict 漂移: {}",
                    review.path
                );
            }
        }
    }
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

    #[test]
    fn archived_legacy_root_keeps_signed_fast_while_modern_history_uses_merge_lane() {
        let signed_fast = vec!["testFast".to_string(), "check".to_string()];
        let resolved_merge = vec![
            "testFast".to_string(),
            "testExclusive".to_string(),
            "check".to_string(),
        ];
        assert_eq!(
            archived_spawned_root_gate_names_v1(true, &signed_fast, &resolved_merge).unwrap(),
            signed_fast
        );
        assert_eq!(
            archived_spawned_root_gate_names_v1(false, &signed_fast, &resolved_merge).unwrap(),
            resolved_merge
        );
        assert!(archived_spawned_root_gate_names_v1(true, &[], &resolved_merge).is_err());
        assert!(archived_spawned_root_gate_names_v1(false, &signed_fast, &[]).is_err());
    }

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
            Path::new("."),
            &events,
            "r48",
            "B130",
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
                Path::new("."),
                &events,
                "r48",
                "B130",
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

            crate::plan::materialize_legacy_plan_fixture(&root).unwrap();
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
            append_historical_root_verdict_fixture(&root, &task_head, &verdict_main);
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

    fn append_historical_root_verdict_fixture(root: &Path, task_head: &str, verdict_main: &str) {
        let round = "r58";
        let task_id = "B172T";
        let attempt_id = "B172T-A0001";
        let ledger_path = root.join("coordination/rounds/r58/events.jsonl");
        let before = read_ledger(&ledger_path).unwrap();
        let active = crate::plan::require_active_round_ir(root, round, &before.events).unwrap();
        let task = active
            .candidate
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .unwrap();
        let lineage =
            current_attempt_and_collect(&before.events, task_id, round, attempt_id, task_head)
                .unwrap();
        let (reviews, evidence, substitutions) = required_file_bindings(
            root,
            &before.events,
            round,
            task_id,
            attempt_id,
            task_head,
            verdict_main,
            &lineage.implementer_agent,
            task,
            RootVerdict::Pass,
            false,
        )
        .unwrap();
        assert!(substitutions.is_empty());
        let binding = committed_attempt_policy_binding(
            root,
            &before.events,
            round,
            task_id,
            attempt_id,
            verdict_main,
        )
        .unwrap();
        let (_, gates) =
            run_verdict_gates(root, round, task_id, attempt_id, task, &binding).unwrap();
        let payload = RootVerdictPayload {
            verdict: "PASS".to_string(),
            reason: None,
            ir_revision: active.persisted_revision,
            validation_digest: active.persisted_digest,
            attempt_id: attempt_id.to_string(),
            attempt_no: lineage.attempt_no,
            implementer_agent: lineage.implementer_agent,
            head_sha: task_head.to_string(),
            main_head_sha: verdict_main.to_string(),
            collect_completed_event_id: lineage.collect.event_id.clone(),
            bootstrap_pre_signoff_attempt: task.bootstrap_pre_signoff_attempt.clone(),
            reviews,
            evidence,
            gates,
        };
        ledger::append(
            root,
            round,
            &[ledger::event(
                "VerdictIssued",
                "verifier:root",
                Some(task_id),
                Some(round),
                serde_json::to_value(payload).unwrap(),
            )],
        )
        .unwrap();
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
    fn legacy_root_verdict_is_rejected_before_the_effect_lock() {
        let root = crate::util::test_scratch_dir("b323-legacy-verdict-generation");
        let runtime = root.join("coordination/runtime");
        let round_dir = root.join("coordination/rounds/r58");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&round_dir).unwrap();
        fs::write(runtime.join("CURRENT-ROUND"), "r58\n").unwrap();
        let ledger_path = round_dir.join("events.jsonl");
        fs::write(&ledger_path, b"").unwrap();
        let before = fs::read(&ledger_path).unwrap();

        let error = run_root_verdict(
            &root,
            "B172T",
            "B172T-A0001",
            &"a".repeat(40),
            &"b".repeat(40),
            RootVerdict::Pass,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("schema 1/2 root verdict writer 已退役"),
            "{error}"
        );
        assert_eq!(fs::read(&ledger_path).unwrap(), before);
        assert!(!runtime.join("locks/merge.lock").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn storage_suffix_accepts_same_round_other_task_and_rejects_cross_round_or_smuggling() {
        let site = MergeSite::new("storage-suffix-scope");
        let current_ledger =
            fs::read(site.root.join("coordination/rounds/r58/events.jsonl")).unwrap();
        site.commit_path("coordination/rounds/r58/events.jsonl", &current_ledger);
        let expected_main = git_output(&site.root, &["rev-parse", "main"]);
        let committed = fs::read(site.root.join("coordination/rounds/r58/events.jsonl")).unwrap();
        let active =
            crate::plan::require_active_round_ir(&site.root, "r58", &site.events()).unwrap();

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
                site.root.join("coordination/rounds/r58/events.jsonl"),
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
                "requestedProvider": null,
                "requestedModel": null,
                "requestedEffort": null,
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
                "requestedProvider": null,
                "requestedModel": null,
                "requestedEffort": null,
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
    fn unified_review_backend_receipt_keeps_exact_action_binding_in_suffix() {
        let site = MergeSite::new("unified-review-receipt-suffix");
        let (wake, review, receipt) = unified_review_receipt_fixture(&site);
        let prior = [wake.clone(), review.clone()];
        assert!(crate::wake::accepted_backend_receipt_matches_wake(&wake, &receipt).unwrap());
        assert!(canonical_wake_backend_receipt(&receipt, &prior, "r58").unwrap());
        // Exercise the committed-suffix reader with an already recorded request.
        // MergeSite supplies a legacy archive fixture, not a live schema3 writer.
        let ledger_rel = "coordination/rounds/r58/events.jsonl";
        let mut bytes = fs::read(site.root.join(ledger_rel)).unwrap();
        for event in [wake, review] {
            bytes.extend(serde_json::to_vec(&event).unwrap());
            bytes.push(b'\n');
        }
        site.commit_path(ledger_rel, &bytes);
        let request_main = git_output(&site.root, &["rev-parse", "main"]);
        bytes.extend(serde_json::to_vec(&receipt).unwrap());
        bytes.push(b'\n');
        fs::write(site.root.join(ledger_rel), bytes).unwrap();
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
                &request_main,
                &active,
                mode,
            )
            .unwrap();
        }
        assert!(validate_expected_main_contract(
            &site.root,
            "r58",
            "B172T",
            &request_main,
            &active,
            CommittedLedgerMode::CanonicalVerdictSuffix,
        )
        .is_err());
        assert!(!events.iter().any(|event| matches!(
            event.kind.as_str(),
            "ManagedWakeTerminated" | "WorkspaceReleased" | "ReviewDelivered" | "TaskRecorded"
        )));
    }

    fn unified_review_receipt_fixture(site: &MergeSite) -> (EventRecord, EventRecord, EventRecord) {
        let identity = serde_json::json!({
            "wakeId": "wake-unified-review",
            "continuationId": "review:r58:B172T:B172T-A0001:review:reviewer-native",
            "attemptId": "B172T-A0001",
            "agent": "reviewer-native",
            "providerKind": "codex",
            "requestedProvider": null,
            "requestedModel": null,
            "requestedEffort": null,
            "requestMessageSha256": "a".repeat(64),
            "renderedMessageSha256": "b".repeat(64),
            "requestSessionId": null,
            "backendState": "pending",
            "logPath": site.root.join("coordination/runtime/logs/unified.jsonl"),
            "probeOffset": 0,
        });
        let binding = serde_json::json!({
            "configDigest": "1".repeat(64),
            "requestDigest": "2".repeat(64),
            "attachmentManifestSha256": "3".repeat(64),
            "commandDigest": "4".repeat(64),
            "executableIdentityDigest": "5".repeat(64),
            "requestedTuple": {"provider": null, "model": null, "effort": null, "mode": null},
            "effectiveTuple": {"provider": null, "model": null, "effort": null, "mode": "read-only"},
            "driver": "codex",
            "harness": "reviewer-native",
            "observationSource": "native-final",
            "invocationCwd": site.root,
            "cwdSelection": "review-site",
            "fixedHead": site.task_head,
        });
        let mut wake_payload = identity.clone();
        wake_payload["method"] = serde_json::json!("unified-channel-v1");
        wake_payload
            .as_object_mut()
            .unwrap()
            .extend(binding.as_object().unwrap().clone());
        let wake = ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            wake_payload,
        );
        let mut review_payload = identity.clone();
        review_payload["role"] = serde_json::json!("review");
        review_payload["channelBinding"] = binding.clone();
        let review = ledger::event(
            "ReviewRequested",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            review_payload,
        );
        let mut receipt_payload = identity;
        receipt_payload["agentEvent"] = serde_json::json!("wake-backend-receipt");
        receipt_payload["actionId"] = serde_json::json!("wake-unified-review");
        receipt_payload["receiptKind"] = serde_json::json!("codex");
        receipt_payload["observedSessionId"] = serde_json::json!("native-review-session");
        receipt_payload["probeEnd"] = serde_json::json!(10);
        receipt_payload["windowSha256"] = serde_json::json!("c".repeat(64));
        receipt_payload["backendState"] = serde_json::json!("accepted");
        receipt_payload["channelBinding"] = binding;
        let receipt = ledger::event(
            "AgentEventReceived",
            "runtime:orch",
            Some("B172T"),
            Some("r58"),
            receipt_payload,
        );
        (wake, review, receipt)
    }

    #[test]
    fn unified_review_backend_receipt_rejects_shape_and_lineage_tampering() {
        let site = MergeSite::new("unified-review-receipt-refusals");
        let (wake, review, receipt) = unified_review_receipt_fixture(&site);
        let prior = [wake.clone(), review.clone()];
        let reject = |changed: &EventRecord, before: &[EventRecord]| {
            assert!(canonical_wake_backend_receipt(changed, before, "r58").is_err());
        };
        for key in receipt
            .payload
            .as_ref()
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
        {
            let mut changed = receipt.clone();
            changed
                .payload
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            if key == "agentEvent" {
                assert!(!canonical_wake_backend_receipt(&changed, &prior, "r58").unwrap());
            } else {
                reject(&changed, &prior);
            }
        }
        let mut extra = receipt.clone();
        extra.payload.as_mut().unwrap()["extra"] = serde_json::json!(true);
        reject(&extra, &prior);
        for key in receipt.payload.as_ref().unwrap()["channelBinding"]
            .as_object()
            .unwrap()
            .keys()
        {
            let mut changed = receipt.clone();
            changed.payload.as_mut().unwrap()["channelBinding"][key] =
                serde_json::json!("tampered");
            reject(&changed, &prior);
        }
        let mut legacy = wake.clone();
        legacy
            .payload
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("method");
        reject(&receipt, &[legacy, review.clone()]);
        let mut malformed_wake = wake.clone();
        malformed_wake.payload.as_mut().unwrap()["configDigest"] = serde_json::json!(false);
        reject(&receipt, &[malformed_wake, review.clone()]);
        reject(&receipt, &[]);
        reject(&receipt, std::slice::from_ref(&wake));
        reject(&receipt, &[wake.clone(), wake.clone(), review.clone()]);
        reject(&receipt, &[wake.clone(), review.clone(), review.clone()]);
        reject(&receipt, &[wake.clone(), review.clone(), receipt.clone()]);
        let mut foreign_review = review.clone();
        foreign_review.payload.as_mut().unwrap()["agent"] = serde_json::json!("another");
        reject(&receipt, &[wake.clone(), foreign_review]);
        let mut unknown_role = receipt.clone();
        unknown_role.payload.as_mut().unwrap()["continuationId"] =
            serde_json::json!("review:r58:B172T:B172T-A0001:unknown:reviewer-native");
        reject(&unknown_role, &prior);
    }

    #[test]
    fn fixture_task_head_remains_fixed() {
        let site = MergeSite::new("fixed-head");
        assert_eq!(
            git_output(&site.root, &["rev-parse", "task/B172T"]),
            site.task_head
        );
    }

    #[test]
    fn root_gate_binding_is_loaded_from_the_dispatch_base() {
        let root = crate::util::test_scratch_dir("b304-root-gate-policy-binding");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "root gate binding"]);
        git(
            &root,
            &["config", "user.email", "root-gate-binding@example.invalid"],
        );
        fs::create_dir_all(root.join("coordination")).unwrap();
        let binding_path = root.join("coordination/PROJECT-BINDING.yaml");
        fs::write(
            &binding_path,
            "commands:\n  testFast:\n    argv: [/bin/sh, -c, 'echo dispatch-base']\n",
        )
        .unwrap();
        git(&root, &["add", "coordination/PROJECT-BINDING.yaml"]);
        git(&root, &["commit", "-q", "-m", "dispatch base binding"]);
        let policy_base_sha = git_output(&root, &["rev-parse", "HEAD"]);

        fs::write(
            &binding_path,
            "commands:\n  testFast:\n    argv: [/bin/sh, -c, 'echo later-main']\n",
        )
        .unwrap();
        git(&root, &["add", "coordination/PROJECT-BINDING.yaml"]);
        git(&root, &["commit", "-q", "-m", "move main binding"]);
        let later_main_sha = git_output(&root, &["rev-parse", "HEAD"]);
        let dispatch = crate::ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B304T"),
            Some("r81t"),
            serde_json::json!({
                "agent": "executor-desktop",
                "attemptId": "B304T-A0001",
                "attemptNo": 1,
                "baseSha": policy_base_sha,
                "goPath": "coordination/rounds/r81t/dispatch/executor-desktop/GO-B304T-A0001.md"
            }),
        );
        let loaded = committed_attempt_policy_binding(
            &root,
            &[dispatch],
            "r81t",
            "B304T",
            "B304T-A0001",
            &later_main_sha,
        )
        .unwrap();
        assert_eq!(
            loaded.commands["testFast"].argv,
            ["/bin/sh", "-c", "echo dispatch-base"]
        );
    }

    #[test]
    fn root_verdict_writer_and_reader_share_the_round_scoped_path() {
        // This test itself invokes a full gate.  Give both fixture levels
        // independent repository identities so an outer production full gate
        // (for example candidate->fast collect) cannot make the inner permit
        // wait on its own parent process forever.
        let outer = crate::util::test_scratch_dir("b272-verdict-scoped-outer");
        git_output(&outer, &["init", "-q"]);
        let root = outer.join("inner");
        fs::create_dir_all(&root).unwrap();
        git_output(&root, &["init", "-q"]);
        let mut outer_permit = gate::WorkspaceFullPermit::acquire(
            &outer,
            "01ARZ3NDEKTSV4RRFFQ69G5FAW",
            "test:verify-round-scoped-outer",
        )
        .unwrap();
        let log_dir = root.join("coordination/runtime/logs");
        fs::create_dir_all(&log_dir).unwrap();
        let spec = binding::CommandSpec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                ": > \"$ORCH_GATE_FIXTURE_REGISTRY\"; echo verdict-green".into(),
            ],
            timeout_seconds: 30,
            trial_timeout_seconds: None,
            approval: None,
        };
        let permit = crate::storage::fixture_gate_permit(
            &root,
            &[root.clone(), log_dir.clone(), root.join("orch/target")],
        );
        let gate_run_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let scoped_tag = gate::phase_scoped_log_tag(
            "r71",
            "B272",
            "B272-A0002",
            gate::GatePhase::Root,
            gate_run_id,
        );
        let result = gate::run_gate_with_permit_and_identity(
            &permit,
            ledger::GateAuditIdentity::Attempt {
                task_id: "B272",
                attempt_id: "B272-A0002",
            },
            "testFast",
            &spec,
            &root,
            &log_dir,
            &scoped_tag,
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.log_path.ends_with(
            "B272-round-r71-B272-A0002-root-01ARZ3NDEKTSV4RRFFQ69G5FAV-gate-testFast.log"
        ));

        let bytes = fs::read(&result.log_path).unwrap();
        let (sha256, len) = sha_binding(&bytes);
        assert_eq!(
            bound_phase_scoped_root_gate_log_bytes(
                &root,
                "r71",
                "B272",
                "B272-A0002",
                gate_run_id,
                "testFast",
                &sha256,
                len,
            )
            .unwrap(),
            bytes
        );

        let legacy = log_dir.join("B272-B272-A0002-root-verdict-gate-testFast.log");
        let old_scoped = log_dir.join(format!(
            "{}-gate-testFast.log",
            root_verdict_scoped_gate_tag("r71", "B272", "B272-A0002")
        ));
        fs::write(&old_scoped, &bytes).unwrap();
        fs::write(&legacy, &bytes).unwrap();
        let error = bound_root_verdict_gate_log_bytes(
            &root,
            "r71",
            "B272",
            "B272-A0002",
            "testFast",
            &sha256,
            len,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("scoped 与 legacy"), "{error}");
        outer_permit.release().unwrap();
    }

    #[test]
    fn root_verdict_legacy_fallback_is_unique_regular_and_byte_bound() {
        let root = crate::util::test_scratch_dir("b272-verdict-legacy-log");
        let log_dir = root.join("coordination/runtime/logs");
        fs::create_dir_all(&log_dir).unwrap();
        let legacy = log_dir.join("B272-B272-A0001-root-verdict-gate-testFast.log");
        let bytes = b"historical verdict gate\n";
        fs::write(&legacy, bytes).unwrap();
        let (sha256, len) = sha_binding(bytes);
        assert_eq!(
            bound_root_verdict_gate_log_bytes(
                &root,
                "r70",
                "B272",
                "B272-A0001",
                "testFast",
                &sha256,
                len,
            )
            .unwrap(),
            bytes
        );

        let alias = log_dir.join("B272-B272-A0001-root-verdict-copy-gate-testFast.log");
        fs::write(&alias, bytes).unwrap();
        let error = bound_root_verdict_gate_log_bytes(
            &root,
            "r70",
            "B272",
            "B272-A0001",
            "testFast",
            &sha256,
            len,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2 个 legacy candidates"), "{error}");
        fs::remove_file(&alias).unwrap();

        let error = bound_root_verdict_gate_log_bytes(
            &root,
            "r70",
            "B272",
            "B272-A0001",
            "testFast",
            &"0".repeat(64),
            len,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("bytes 已漂移"), "{error}");

        let error = bound_root_verdict_gate_log_bytes(
            &root,
            "r70",
            "B272",
            "B272-A0001",
            "testFast",
            &sha256,
            len + 1,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("bytes 已漂移"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn root_verdict_legacy_symlink_and_missing_candidates_fail_closed() {
        use std::os::unix::fs::symlink;

        let root = crate::util::test_scratch_dir("b272-verdict-legacy-symlink");
        let log_dir = root.join("coordination/runtime/logs");
        fs::create_dir_all(&log_dir).unwrap();
        let missing = bound_root_verdict_gate_log_bytes(
            &root,
            "r70",
            "B272",
            "B272-A0001",
            "testFast",
            &"0".repeat(64),
            0,
        )
        .unwrap_err()
        .to_string();
        assert!(missing.contains("无 scoped/legacy candidate"), "{missing}");

        let target = root.join("outside.log");
        fs::write(&target, b"target\n").unwrap();
        let scoped = log_dir.join("B272-B272-A0001-root-verdict-round-r70-gate-testFast.log");
        symlink(&target, &scoped).unwrap();
        let error = bound_root_verdict_gate_log_bytes(
            &root,
            "r70",
            "B272",
            "B272-A0001",
            "testFast",
            &"0".repeat(64),
            0,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("scoped root verdict gate log 不是 regular file"),
            "{error}"
        );
        fs::remove_file(&scoped).unwrap();

        let legacy = log_dir.join("B272-B272-A0001-root-verdict-gate-testFast.log");
        symlink(&target, &legacy).unwrap();
        let error = bound_root_verdict_gate_log_bytes(
            &root,
            "r70",
            "B272",
            "B272-A0001",
            "testFast",
            &"0".repeat(64),
            0,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("不是 regular file"), "{error}");
    }
}
