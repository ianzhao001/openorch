use super::*;

#[test]
fn exact_terminal_collect_rejection_builds_repair_prompt() {
    let site = test_root("resume-after-collect-rejection");
    let (attempt, _go_rel, _base_sha) = append_modern_dispatch(&site);
    let (report_rel, report, _branch_sha) = commit_report(&site);
    let control_epoch = read_events(&site.root)
        .iter()
        .rev()
        .find(|event| event.kind == "DispatchIssued")
        .unwrap()
        .event_id
        .clone();
    let evidence_path = site.root.join(&report_rel).to_string_lossy().into_owned();
    let evidence_sha256 = hex::encode(sha2::Sha256::digest(&report));
    let collect_payload = serde_json::json!({
        "actionId": "collect-action",
        "attemptId": attempt.attempt_id,
        "attemptNo": attempt.ordinal,
        "evidencePath": evidence_path,
        "evidenceSha256": evidence_sha256,
        "evidenceLen": report.len(),
        "controlEpoch": control_epoch,
        "owner": "collect-owner",
        "leaseGeneration": "collect-generation",
    });
    ledger::append(
        &site.root,
        "r44",
        &[
            ledger::event(
                "ReportObserved",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                serde_json::json!({
                    "actionId": "report-observed",
                    "attemptId": "B97-A0001",
                    "attemptNo": 1,
                    "evidencePath": evidence_path,
                    "evidenceSha256": evidence_sha256,
                    "evidenceLen": report.len(),
                    "controlEpoch": control_epoch,
                }),
            ),
            ledger::event(
                "ReportCollectClaimed",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                collect_payload.clone(),
            ),
            ledger::event(
                "ReportCollectExecuting",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                collect_payload.clone(),
            ),
            ledger::event(
                "MechCheckFailed",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                serde_json::json!({
                    "stage": "frozen",
                    "reason": "task branch modified landed frozen seed",
                }),
            ),
            ledger::event(
                "ReportCollectReleased",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                collect_payload,
            ),
            ledger::event(
                "ActionRejected",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                serde_json::json!({
                    "actionId": "await-report:r44:B97",
                    "operation": "await-report",
                    "attemptId": "B97-A0001",
                    "attemptNo": 1,
                    "exitCode": 4,
                    "alert": true,
                    "reason": "task branch modified landed frozen seed",
                }),
            ),
        ],
    )
    .unwrap();

    let card = card::load(&site.root, "r44", "B97").unwrap();
    let plan =
        build_resume_plan_from_events(&site.root, "r44", "B97", &card, &read_events(&site.root))
            .unwrap();
    assert!(plan.prompt.contains("collect 终态拒绝"));
    assert!(plan.prompt.contains("stage=\"frozen\""));
    assert!(plan
        .prompt
        .contains("reason=\"task branch modified landed frozen seed\""));
    assert!(plan.prompt.contains("逐字节逆向编辑恢复冻结 seed"));
    assert!(plan.prompt.contains("返修完成后必须最后更新"));
    assert!(!plan.prompt.contains("无需重启执行会话"));
}

fn append_terminal_collect_rejection(
    site: &TierfRoot,
    attempt: &crate::attempt::AttemptRef,
    report_rel: &str,
    report: &[u8],
    control_epoch: &str,
    stage: &str,
    reason: &str,
) {
    let evidence_path = site.root.join(report_rel).to_string_lossy().into_owned();
    let evidence_sha256 = hex::encode(sha2::Sha256::digest(report));
    let collect_payload = serde_json::json!({
        "actionId": format!("collect-{stage}"),
        "attemptId": attempt.attempt_id,
        "attemptNo": attempt.ordinal,
        "evidencePath": evidence_path,
        "evidenceSha256": evidence_sha256,
        "evidenceLen": report.len(),
        "controlEpoch": control_epoch,
        "owner": format!("owner-{stage}"),
        "leaseGeneration": format!("generation-{stage}"),
    });
    ledger::append(
        &site.root,
        "r44",
        &[
            ledger::event(
                "ReportObserved",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                serde_json::json!({
                    "actionId": "report-observed",
                    "attemptId": attempt.attempt_id,
                    "attemptNo": attempt.ordinal,
                    "evidencePath": evidence_path,
                    "evidenceSha256": evidence_sha256,
                    "evidenceLen": report.len(),
                    "controlEpoch": control_epoch,
                }),
            ),
            ledger::event(
                "ReportCollectClaimed",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                collect_payload.clone(),
            ),
            ledger::event(
                "ReportCollectExecuting",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                collect_payload.clone(),
            ),
            ledger::event(
                "MechCheckFailed",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                serde_json::json!({"stage": stage, "reason": reason}),
            ),
            ledger::event(
                "ReportCollectReleased",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                collect_payload,
            ),
            ledger::event(
                "ActionRejected",
                "runtime:orch",
                Some("B97"),
                Some("r44"),
                serde_json::json!({
                    "actionId": "await-report:r44:B97",
                    "operation": "await-report",
                    "attemptId": attempt.attempt_id,
                    "attemptNo": attempt.ordinal,
                    "exitCode": 4,
                    "alert": true,
                    "reason": reason,
                }),
            ),
        ],
    )
    .unwrap();
}

fn replace_committed_report(
    site: &TierfRoot,
    report_rel: &str,
    report: &[u8],
    subject: &str,
) -> String {
    test_git(&site.root, &["checkout", "-q", "task/B97"]);
    std::fs::write(site.root.join(report_rel), report).unwrap();
    test_git(&site.root, &["add", report_rel]);
    test_git(
        &site.root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            subject,
        ],
    );
    let tip = test_git(&site.root, &["rev-parse", "HEAD"]);
    test_git(&site.root, &["checkout", "-q", "main"]);
    tip
}

#[test]
fn post_resume_report_epoch_preserves_the_latest_rejection_in_a_fresh_generation() {
    let site = test_root("post-resume-report-epoch");
    let (attempt, _go_rel, _base_sha) = append_modern_dispatch(&site);
    let (report_rel, report_one, tip_one) = commit_report(&site);
    let dispatch_epoch = read_events(&site.root)
        .iter()
        .rev()
        .find(|event| event.kind == "DispatchIssued")
        .unwrap()
        .event_id
        .clone();
    let old_reason = "OLD-DISPATCH-EPOCH-REASON";
    append_terminal_collect_rejection(
        &site,
        &attempt,
        &report_rel,
        &report_one,
        &dispatch_epoch,
        "first-rejection",
        old_reason,
    );

    let (_, prompt_one) = run_resume_with_hook(&site.root, "B97", &mut |_| Ok(())).unwrap();
    wait_for_lines(&site.wake_marker, 1);
    assert!(prompt_one.contains(old_reason));
    assert!(prompt_one.contains(&tip_one[..7]));
    let resume_epoch = read_events(&site.root)
        .iter()
        .rev()
        .find(|event| event.kind == "ResumeIssued")
        .unwrap()
        .event_id
        .clone();

    let report_two = b"## 0 execution\nMODEL=test\nDEPTH=high\nCAPTURE=post-resume-epoch\n\n## 1 changes\nsecond rejection\n";
    let tip_two = replace_committed_report(&site, &report_rel, report_two, "report generation two");
    let latest_reason = "LATEST-RESUME-EPOCH-REASON";
    append_terminal_collect_rejection(
        &site,
        &attempt,
        &report_rel,
        report_two,
        &resume_epoch,
        "second-rejection",
        latest_reason,
    );

    let (_, prompt_two) = run_resume_with_hook(&site.root, "B97", &mut |_| Ok(())).unwrap();
    wait_for_lines(&site.wake_marker, 2);
    assert_ne!(prompt_one, prompt_two);
    assert!(prompt_two.contains(latest_reason));
    assert!(prompt_two.contains(&tip_two[..7]));
    assert!(!prompt_two.contains(old_reason));
    assert!(!prompt_two.contains(&tip_one[..7]));
    assert!(!prompt_two.contains("无需重启执行会话"));
    let issued = read_events(&site.root)
        .into_iter()
        .filter(|event| event.kind == "ResumeIssued")
        .collect::<Vec<_>>();
    assert_eq!(issued.len(), 2);
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
fn resume_wake_wiring_uses_the_controlled_replacement_entry() {
    let source = include_str!("../../tierf.rs");
    let resume = source
        .split_once("fn run_resume_with_hook_effect(")
        .unwrap()
        .1;
    assert!(resume.contains("wake::dispatch_resume_wake_for_continuation("));
    assert!(resume.contains("action_id,\n            owned_by,\n            owned_generation,"));
    assert!(!resume.contains(
        "wake::dispatch_wake_for_continuation(\n            root,\n            &plan.agent"
    ));
}

fn resume_generation_fixture(prompt: &str) -> ResumePlan {
    let digest = hex::encode(sha2::Sha256::digest(prompt.as_bytes()));
    let attempt_id = "B97-A0001".to_string();
    let attempt_no = 1;
    let agent = "executor-desktop".to_string();
    let base_sha = "a".repeat(40);
    let go_path = "coordination/rounds/r44/dispatch/executor-desktop/GO-B97-A0001.md".to_string();
    let action_id = resume_action_id(
        "r44",
        "B97",
        &attempt_id,
        attempt_no,
        &agent,
        &base_sha,
        &go_path,
        &digest,
    );
    ResumePlan {
        attempt_id,
        attempt_no,
        agent,
        base_sha,
        go_path,
        prompt: prompt.to_string(),
        digest,
        action_id,
    }
}

fn resume_issued_fixture(plan: &ResumePlan) -> EventRecord {
    ledger::event(
        "ResumeIssued",
        "runtime:orch",
        Some("B97"),
        Some("r44"),
        serde_json::json!({
            "attemptId": plan.attempt_id,
            "attemptNo": plan.attempt_no,
            "agent": plan.agent,
            "baseSha": plan.base_sha,
            "goPath": plan.go_path,
            "prompt": plan.prompt,
            "resumeDigest": plan.digest,
            "actionId": plan.action_id,
            "wakePending": true,
        }),
    )
}

#[test]
fn stale_pending_generation_cannot_override_the_current_plan() {
    let stale = resume_generation_fixture("stale prompt");
    let current = resume_generation_fixture("current prompt");
    let mut events = vec![
        ledger::event(
            "DispatchIssued",
            "runtime:orch",
            Some("B97"),
            Some("r44"),
            serde_json::json!({"attemptId": "B97-A0001"}),
        ),
        resume_issued_fixture(&stale),
    ];
    let fresh = resume_ledger_state(&events, "B97", "r44", &current).unwrap();
    assert_eq!(fresh.state, ResumeLedgerState::New);
    assert_eq!(fresh.plan.action_id, current.action_id);

    events.push(resume_issued_fixture(&current));
    let view = resume_ledger_state(&events, "B97", "r44", &current).unwrap();
    assert_eq!(view.state, ResumeLedgerState::Pending);
    assert_eq!(view.plan.action_id, current.action_id);
    assert_ne!(view.plan.action_id, stale.action_id);
}
