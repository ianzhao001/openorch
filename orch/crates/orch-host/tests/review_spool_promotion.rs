use orch_host::ledger::{self, RuntimeEventPayloadV1};
use orch_host::verify::{check_review_artifact_contract, ReviewContractExpectation};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const ROUND: &str = "r81";
const TASK: &str = "B900";
const ATTEMPT: &str = "B900-A0001";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn panel_review() -> Vec<u8> {
    format!(
        "---\ntaskId: {TASK}\nround: {ROUND}\nattemptId: {ATTEMPT}\nrole: primary\nreviewer: executor-one\nverdict: PASS\nreviewedHead: {HEAD}\nseatId: seat-one\ngeneration: 1\nwakeId: wake-one\npolicyBaseSha: {BASE}\nextraAudit: retained\n---\nsubstantive finding\n"
    )
    .into_bytes()
}

#[test]
fn panel_frontmatter_binds_seat_generation_wake_and_policy_base() {
    let expected = ReviewContractExpectation::panel_exact(
        TASK,
        ROUND,
        ATTEMPT,
        "primary",
        "executor-one",
        HEAD,
        "seat-one",
        1,
        "wake-one",
        BASE,
    )
    .unwrap();
    let checked = check_review_artifact_contract(&panel_review(), &expected)
        .unwrap()
        .unwrap();
    assert_eq!(checked.verdict(), "PASS");
    assert_eq!(checked.ignored_fields(), &["extraAudit".to_string()]);

    for (from, to) in [
        ("seatId: seat-one", "seatId: seat-wrong"),
        ("generation: 1", "generation: 2"),
        ("wakeId: wake-one", "wakeId: wake-wrong"),
        (
            &format!("policyBaseSha: {BASE}"),
            "policyBaseSha: cccccccccccccccccccccccccccccccccccccccc",
        ),
    ] {
        let mutated = String::from_utf8(panel_review())
            .unwrap()
            .replace(from, to)
            .into_bytes();
        assert!(check_review_artifact_contract(&mutated, &expected).is_err());
    }
}

fn routed_events() -> Vec<orch_core::EventRecord> {
    let dispatch = ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "attemptId": ATTEMPT,
            "attemptNo": 1,
            "agent": "executor-implementer",
            "baseSha": BASE,
        }),
    );
    let collect = ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "attemptId": ATTEMPT,
            "branchSha": HEAD,
        }),
    );
    let selected_payload = ledger::ReviewPanelSelectedPayloadV1 {
        schema_version: 1,
        panel_id: "panel-one".to_string(),
        attempt_id: ATTEMPT.to_string(),
        attempt_no: 1,
        reviewed_head: HEAD.to_string(),
        policy_base_sha: BASE.to_string(),
        policy: "review-pool-v1".to_string(),
        policy_sha256: "d".repeat(64),
        seat_count: 3,
        seat_ids: [
            "seat-one".to_string(),
            "seat-two".to_string(),
            "seat-three".to_string(),
        ],
    };
    let selected = ledger::runtime_event_v1(
        ROUND,
        Some(TASK),
        RuntimeEventPayloadV1::ReviewPanelSelected(selected_payload),
    )
    .unwrap();
    let make_route = |seat: &str, wake: &str, role: &str, agent: &str| {
        ledger::runtime_event_v1(
            ROUND,
            Some(TASK),
            RuntimeEventPayloadV1::ReviewSeatRouted(ledger::ReviewSeatRoutedPayloadV1 {
                schema_version: 1,
                panel_id: "panel-one".to_string(),
                seat_id: seat.to_string(),
                generation: 1,
                wake_id: wake.to_string(),
                attempt_id: ATTEMPT.to_string(),
                attempt_no: 1,
                role: role.to_string(),
                agent: agent.to_string(),
                lineage: role.to_string(),
                reviewed_head: HEAD.to_string(),
                policy_base_sha: BASE.to_string(),
                deadline_secs: 3_600,
                retry_eligible: true,
                route_kind: "initial".to_string(),
                selected_event_id: selected.event_id.clone(),
                source_seat_id: None,
                source_generation: None,
                source_terminal_event_id: None,
            }),
        )
        .unwrap()
    };
    let one = make_route("seat-one", "wake-one", "primary", "executor-one");
    let two = make_route("seat-two", "wake-two", "secondary", "executor-two");
    let three = make_route("seat-three", "wake-three", "nongate", "executor-three");
    let terminal = {
        let mut event = ledger::event(
            "ManagedWakeTerminated",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "wakeId": "wake-one",
                "agent": "executor-one",
                "state": "answered",
                "outputPath": "/repo/.worktrees/review/.cowork-temp/review-spool/B900-A0001-seat-one-g1-wake-one.md",
                "outputSha256": "e".repeat(64),
                "managedScopeTerminated": true,
            }),
        );
        event.event_id = "terminal-one".to_string();
        event
    };
    let delivery = {
        let mut event = ledger::event(
            "ReviewDelivered",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "attemptId": ATTEMPT,
                "role": "primary",
                "agent": "executor-one",
                "bodyLen": 10,
                "panelId": "panel-one",
                "seatId": "seat-one",
                "generation": 1,
                "wakeId": "wake-one",
                "policyBaseSha": BASE,
                "reviewedHead": HEAD,
            }),
        );
        event.event_id = "delivery-one".to_string();
        event
    };
    let promotion = ledger::runtime_event_v1(
        ROUND,
        Some(TASK),
        RuntimeEventPayloadV1::ReviewSpoolPromoted(ledger::ReviewSpoolPromotedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-one".to_string(),
            seat_id: "seat-one".to_string(),
            generation: 1,
            wake_id: "wake-one".to_string(),
            attempt_id: ATTEMPT.to_string(),
            attempt_no: 1,
            role: "primary".to_string(),
            agent: "executor-one".to_string(),
            reviewed_head: HEAD.to_string(),
            policy_base_sha: BASE.to_string(),
            staging_path: ".worktrees/review/.cowork-temp/review-spool/B900-A0001-seat-one-g1-wake-one.md".to_string(),
            canonical_path: "coordination/rounds/r81/reviews/B900-A0001-seat-one-g1-wake-one.md".to_string(),
            sha256: "e".repeat(64),
            bytes: 100,
            body_len: 10,
            verdict: "PASS".to_string(),
            terminal_event_id: "terminal-one".to_string(),
            delivery_event_id: "delivery-one".to_string(),
        }),
    )
    .unwrap();
    let seat_terminal = ledger::runtime_event_v1(
        ROUND,
        Some(TASK),
        RuntimeEventPayloadV1::ReviewSeatTerminated(ledger::ReviewSeatTerminatedPayloadV1 {
            schema_version: 1,
            panel_id: "panel-one".to_string(),
            seat_id: "seat-one".to_string(),
            generation: 1,
            wake_id: "wake-one".to_string(),
            attempt_id: ATTEMPT.to_string(),
            attempt_no: 1,
            role: "primary".to_string(),
            agent: "executor-one".to_string(),
            lineage: "primary".to_string(),
            reviewed_head: HEAD.to_string(),
            policy_base_sha: BASE.to_string(),
            state: "pass".to_string(),
            terminal_event_id: "terminal-one".to_string(),
            delivery_event_id: Some("delivery-one".to_string()),
            reason: "substantive PASS".to_string(),
        }),
    )
    .unwrap();
    vec![
        dispatch,
        collect,
        selected,
        one,
        two,
        three,
        terminal,
        promotion,
        delivery,
        seat_terminal,
    ]
}

#[test]
fn promotion_history_binds_route_terminal_delivery_and_digest() {
    let events = routed_events();
    ledger::validate_runtime_event_history_v1(&events, ROUND).unwrap();

    let mut wrong_sha = events.clone();
    wrong_sha[6].payload.as_mut().unwrap()["outputSha256"] = serde_json::json!("f".repeat(64));
    assert!(ledger::validate_runtime_event_history_v1(&wrong_sha, ROUND).is_err());

    let mut wrong_generation = events;
    wrong_generation[7].payload.as_mut().unwrap()["generation"] = serde_json::json!(2);
    assert!(ledger::validate_runtime_event_history_v1(&wrong_generation, ROUND).is_err());
}

fn assert_history_mutation_rejected<F>(
    baseline: &[orch_core::EventRecord],
    label: &str,
    mutate: F,
) where
    F: FnOnce(&mut Vec<orch_core::EventRecord>),
{
    ledger::validate_runtime_event_history_v1(baseline, ROUND)
        .unwrap_or_else(|error| panic!("{label} baseline must be green: {error:#}"));
    let mut changed = baseline.to_vec();
    mutate(&mut changed);
    assert!(
        ledger::validate_runtime_event_history_v1(&changed, ROUND).is_err(),
        "{label} mutation must fail closed"
    );
    ledger::validate_runtime_event_history_v1(baseline, ROUND)
        .unwrap_or_else(|error| panic!("{label} restore must be green: {error:#}"));
}

#[test]
fn m1_m18_closed_authority_mutations_fail_and_restore_green() {
    let baseline = routed_events();
    let mut proofs = 0usize;
    macro_rules! mutation {
        ($label:literal, $body:expr) => {{
            assert_history_mutation_rejected(&baseline, $label, $body);
            proofs += 1;
        }};
    }

    mutation!("M1 selected actor", |events| {
        events[2].actor = "user".to_string();
    });
    mutation!("M2 selected unknown payload key", |events| {
        events[2].payload.as_mut().unwrap()["unknown"] = serde_json::json!(true);
    });
    mutation!("M3 selected schema", |events| {
        events[2].payload.as_mut().unwrap()["schemaVersion"] = serde_json::json!(2);
    });
    mutation!("M4 selected seat count", |events| {
        events[2].payload.as_mut().unwrap()["seatCount"] = serde_json::json!(2);
    });
    mutation!("M5 duplicate selected seat", |events| {
        events[2].payload.as_mut().unwrap()["seatIds"] =
            serde_json::json!(["seat-one", "seat-one", "seat-three"]);
    });
    mutation!("M6 selected policy base", |events| {
        events[2].payload.as_mut().unwrap()["policyBaseSha"] =
            serde_json::json!("c".repeat(40));
    });
    mutation!("M7 non-adjacent initial routes", |events| {
        events.insert(
            3,
            ledger::event(
                "ReportObserved",
                "runtime:orch",
                Some(TASK),
                Some(ROUND),
                serde_json::json!({"attemptId": ATTEMPT}),
            ),
        );
    });
    mutation!("M8 duplicate initial agent", |events| {
        events[4].payload.as_mut().unwrap()["agent"] = serde_json::json!("executor-one");
    });
    mutation!("M9 initial generation", |events| {
        events[3].payload.as_mut().unwrap()["generation"] = serde_json::json!(2);
    });
    mutation!("M10 duplicate route wake", |events| {
        events[4].payload.as_mut().unwrap()["wakeId"] = serde_json::json!("wake-one");
    });
    mutation!("M11 route role lineage", |events| {
        events[4].payload.as_mut().unwrap()["lineage"] = serde_json::json!("primary");
    });
    mutation!("M12 unmanaged terminal scope", |events| {
        events[6].payload.as_mut().unwrap()["managedScopeTerminated"] =
            serde_json::json!(false);
    });
    mutation!("M13 terminal state mismatch", |events| {
        events[6].payload.as_mut().unwrap()["state"] = serde_json::json!("failed");
    });
    mutation!("M14 seat terminal agent", |events| {
        events[9].payload.as_mut().unwrap()["agent"] = serde_json::json!("executor-wrong");
    });
    mutation!("M15 promotion digest", |events| {
        events[7].payload.as_mut().unwrap()["sha256"] = serde_json::json!("f".repeat(64));
    });
    mutation!("M16 promotion delivery", |events| {
        events[7].payload.as_mut().unwrap()["deliveryEventId"] =
            serde_json::json!("delivery-wrong");
    });
    mutation!("M17 promotion canonical path", |events| {
        events[7].payload.as_mut().unwrap()["canonicalPath"] =
            serde_json::json!("coordination/rounds/r81/reviews/wrong.md");
    });
    mutation!("M18 duplicate seat terminal", |events| {
        let mut duplicate = events[9].clone();
        duplicate.event_id = "seat-terminal-duplicate".to_string();
        events.push(duplicate);
    });
    assert_eq!(proofs, 18);
}

#[test]
fn late_duplicate_promotion_state_drift_and_wake_before_route_fail_closed() {
    let baseline = routed_events();
    ledger::validate_runtime_event_history_v1(&baseline, ROUND).unwrap();

    let mut late_duplicate = baseline.clone();
    let mut promotion = late_duplicate[7].clone();
    promotion.event_id = "promotion-two".to_string();
    promotion.payload.as_mut().unwrap()["deliveryEventId"] =
        serde_json::json!("delivery-two");
    let mut delivery = late_duplicate[8].clone();
    delivery.event_id = "delivery-two".to_string();
    late_duplicate.extend([promotion, delivery]);
    assert!(ledger::validate_runtime_event_history_v1(&late_duplicate, ROUND).is_err());

    let mut state_drift = baseline.clone();
    state_drift[9].payload.as_mut().unwrap()["state"] = serde_json::json!("fail");
    assert!(ledger::validate_runtime_event_history_v1(&state_drift, ROUND).is_err());

    let mut early_wake = baseline.clone();
    early_wake.insert(
        2,
        ledger::event(
            "WakeIssued",
            "runtime:orch",
            Some(TASK),
            Some(ROUND),
            serde_json::json!({
                "wakeId": "wake-one",
                "attemptId": ATTEMPT,
                "agent": "executor-one",
            }),
        ),
    );
    assert!(ledger::validate_runtime_event_history_v1(&early_wake, ROUND).is_err());

    let route_event_id = baseline[3].event_id.clone();
    let panel_wake = ledger::event(
        "WakeIssued",
        "runtime:orch",
        Some(TASK),
        Some(ROUND),
        serde_json::json!({
            "wakeId": "wake-one",
            "attemptId": ATTEMPT,
            "agent": "executor-one",
            "panelId": "panel-one",
            "seatId": "seat-one",
            "generation": 1,
            "routeEventId": route_event_id,
            "policyBaseSha": BASE,
            "reviewedHead": HEAD,
        }),
    );
    let mut exact_wake = baseline.clone();
    exact_wake.insert(6, panel_wake.clone());
    ledger::validate_runtime_event_history_v1(&exact_wake, ROUND).unwrap();
    let mut wrong_route = panel_wake;
    wrong_route.payload.as_mut().unwrap()["routeEventId"] =
        serde_json::json!("route-wrong");
    let mut wrong_route_history = baseline.clone();
    wrong_route_history.insert(6, wrong_route);
    assert!(ledger::validate_runtime_event_history_v1(&wrong_route_history, ROUND).is_err());

    let mut head_drift = baseline;
    head_drift[2].payload.as_mut().unwrap()["reviewedHead"] =
        serde_json::json!("c".repeat(40));
    assert!(ledger::validate_runtime_event_history_v1(&head_drift, ROUND).is_err());
}

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn write_events(root: &Path, events: &[orch_core::EventRecord]) {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).unwrap();
        bytes.push(b'\n');
    }
    fs::write(root.join("coordination/rounds/r81/events.jsonl"), bytes).unwrap();
}

fn install_reference_transaction_hook(root: &Path) {
    let hook_dir = root.join(".git/b310-hooks");
    fs::create_dir_all(&hook_dir).unwrap();
    let hook = hook_dir.join("reference-transaction");
    fs::write(
        &hook,
        include_bytes!("../../../../.githooks/reference-transaction"),
    )
    .unwrap();
    let mut permissions = fs::metadata(&hook).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&hook, permissions).unwrap();
    assert!(git(
        root,
        &["config", "core.hooksPath", hook_dir.to_str().unwrap()],
    )
    .status
    .success());
}

#[test]
fn reference_transaction_accepts_the_exact_batch_and_rejects_late_repromotion() {
    let root = orch_host::util::test_scratch_dir("b310-review-hook-relations");
    assert!(git(&root, &["init", "-q", "-b", "main"]).status.success());
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r81")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r81\n").unwrap();
    let events = routed_events();
    write_events(&root, &events[..2]);
    assert!(git(&root, &["add", "coordination"]).status.success());
    assert!(git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "base ledger",
        ],
    )
    .status
    .success());

    install_reference_transaction_hook(&root);

    write_events(&root, &events);
    assert!(git(&root, &["add", "coordination/rounds/r81/events.jsonl"])
        .status
        .success());
    let valid = git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "valid panel batch",
        ],
    );
    assert!(
        valid.status.success(),
        "valid hook batch rejected: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let valid_head = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    let mut duplicate = events;
    let mut promotion = duplicate[7].clone();
    promotion.event_id = "promotion-two".to_string();
    promotion.payload.as_mut().unwrap()["deliveryEventId"] =
        serde_json::json!("delivery-two");
    let mut delivery = duplicate[8].clone();
    delivery.event_id = "delivery-two".to_string();
    duplicate.extend([promotion, delivery]);
    write_events(&root, &duplicate);
    assert!(git(&root, &["add", "coordination/rounds/r81/events.jsonl"])
        .status
        .success());
    let invalid = git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "late duplicate promotion",
        ],
    );
    assert!(!invalid.status.success());
    assert_eq!(
        String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim(),
        valid_head
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn reference_transaction_rejects_policy_activation_without_exact_owner_history() {
    let root = orch_host::util::test_scratch_dir("b310-policy-hook-owner");
    assert!(git(&root, &["init", "-q", "-b", "main"]).status.success());
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r81")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r81\n").unwrap();
    let base_event = ledger::event(
        "TaskValidated",
        "runtime:orch",
        None,
        Some(ROUND),
        serde_json::json!({"irRevision": 1, "validationDigest": "a".repeat(64)}),
    );
    write_events(&root, std::slice::from_ref(&base_event));
    assert!(git(&root, &["add", "coordination"]).status.success());
    assert!(git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "base ledger",
        ],
    )
    .status
    .success());
    let base_head = String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    install_reference_transaction_hook(&root);

    let activation = ledger::runtime_event_v1(
        ROUND,
        None,
        RuntimeEventPayloadV1::RuntimePolicyActivated(
            ledger::RuntimePolicyActivatedPayloadV1 {
                schema_version: 1,
                policy: "review-pool-v1".to_string(),
                owner_task: "B310".to_string(),
                owner_recorded_event_id: "missing-recorded".to_string(),
                owner_merge_sha: "b".repeat(40),
                binding_sha256: "c".repeat(64),
                policy_sha256: "d".repeat(64),
                activated_at_main_sha: base_head.clone(),
            },
        ),
    )
    .unwrap();
    write_events(&root, &[base_event, activation]);
    assert!(git(&root, &["add", "coordination/rounds/r81/events.jsonl"])
        .status
        .success());
    let invalid = git(
        &root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch-test@example.invalid",
            "commit",
            "-q",
            "-m",
            "orphan policy activation",
        ],
    );
    assert!(!invalid.status.success());
    assert_eq!(
        String::from_utf8(git(&root, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim(),
        base_head
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn reference_transaction_idle_projection_is_set_membership_idempotent() {
    let hook = include_str!("../../../../.githooks/reference-transaction");
    let start = hook
        .find("        function open_attempt(")
        .expect("hook open_attempt helper");
    let end = hook[start..]
        .find("        # B310 hook-side closed shape")
        .map(|offset| start + offset)
        .expect("hook idle helper boundary");
    let mut program = hook[start..end].to_string();
    program.push_str(
        r#"
BEGIN {
    open_attempt("B306", "B306-A0001")
    close_attempt("B306", "B306-A0001")
    close_attempt("B306", "B306-A0001")
    close_task_attempts("B306")
    open_wake_id("wake-one", "B306")
    close_wake_id("wake-one")
    close_wake_id("wake-one")
    if (open_attempt_count != 0 || open_wake_count != 0 ||
        (("B306|B306-A0001") in attempt_open) || ("wake-one" in wake_open)) {
        print "attempts=" open_attempt_count " wakes=" open_wake_count
        exit 1
    }
}
"#,
    );

    let root = orch_host::util::test_scratch_dir("b310-hook-idle-membership");
    fs::create_dir_all(&root).unwrap();
    let script = root.join("idle-projection.awk");
    fs::write(&script, program).unwrap();
    let output = Command::new("/usr/bin/awk")
        .args(["-f", script.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "idle projection underflowed or retained a ghost key: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn reference_transaction_task_recorded_sweeps_only_its_wakes() {
    let hook = include_str!("../../../../.githooks/reference-transaction");
    let start = hook
        .find("        function open_attempt(")
        .expect("hook open_attempt helper");
    let end = hook[start..]
        .find("        # B310 hook-side closed shape")
        .map(|offset| start + offset)
        .expect("hook idle helper boundary");
    let mut program = hook[start..end].to_string();
    program.push_str(
        r#"
BEGIN {
    open_wake_id("wake-b305-one", "B305")
    open_wake_id("wake-b305-two", "B305")
    open_wake_id("wake-b304", "B304")
    close_task_wakes("B305")
    if (open_wake_count != 1 || ("wake-b305-one" in wake_open) ||
        ("wake-b305-two" in wake_open) || !("wake-b304" in wake_open) ||
        ("wake-b305-one" in wake_task) || ("wake-b305-two" in wake_task) ||
        wake_task["wake-b304"] != "B304") {
        print "after-b305 wakes=" open_wake_count
        exit 1
    }
    close_task_wakes("B304")
    if (open_wake_count != 0 || ("wake-b304" in wake_open) ||
        ("wake-b304" in wake_task)) {
        print "after-b304 wakes=" open_wake_count
        exit 1
    }
}
"#,
    );

    let task_recorded_start = hook
        .find("            } else if (kind == \"TaskRecorded\") {")
        .expect("hook TaskRecorded branch");
    let task_recorded_end = hook[task_recorded_start..]
        .find("            } else if (kind == \"AttemptBlocked\"")
        .map(|offset| task_recorded_start + offset)
        .expect("hook TaskRecorded branch end");
    let task_recorded = &hook[task_recorded_start..task_recorded_end];
    assert!(task_recorded.contains("payload_string[\"postMergeGates\"] == \"all-green\""));
    assert!(task_recorded.contains("close_task_wakes(task)"));

    let root = orch_host::util::test_scratch_dir("b310-hook-task-wake-sweep");
    fs::create_dir_all(&root).unwrap();
    let script = root.join("task-wake-sweep.awk");
    fs::write(&script, program).unwrap();
    let output = Command::new("/usr/bin/awk")
        .args(["-f", script.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "task-scoped wake sweep leaked or crossed task boundary: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn no_clobber_is_identical_replay_only_and_rejects_symlinks() {
    let root = orch_host::util::test_scratch_dir("b310-review-no-clobber");
    let rel = "coordination/rounds/r81/reviews/B900-A0001-seat-one-g1-wake-one.md";
    orch_host::wake::install_review_no_clobber_for_test(&root, rel, b"first\n").unwrap();
    orch_host::wake::install_review_no_clobber_for_test(&root, rel, b"first\n").unwrap();
    assert!(orch_host::wake::install_review_no_clobber_for_test(&root, rel, b"second\n").is_err());

    let symlink_rel = "coordination/rounds/r81/reviews/B900-A0001-seat-two-g1-wake-two.md";
    let symlink_path = root.join(symlink_rel);
    fs::write(root.join("target.md"), b"target\n").unwrap();
    std::os::unix::fs::symlink(root.join("target.md"), &symlink_path).unwrap();
    assert!(orch_host::wake::install_review_no_clobber_for_test(
        &root,
        symlink_rel,
        b"target\n"
    )
    .is_err());
    fs::remove_dir_all(root).ok();
}

#[test]
fn staging_path_is_bound_to_the_exact_workspace_lease() {
    let artifact = "B900-A0001-seat-one-g1-wake-one.md";
    let leased = ".worktrees/review-B900-A0001-primary-executor-one-g01";
    let exact = format!("{leased}/.cowork-temp/review-spool/{artifact}");
    orch_host::wake::validate_panel_staging_lease_for_test(leased, &exact, artifact).unwrap();

    let wrong_worktree = format!(
        ".worktrees/review-B900-A0001-primary-executor-two-g01/.cowork-temp/review-spool/{artifact}"
    );
    assert!(orch_host::wake::validate_panel_staging_lease_for_test(
        leased,
        &wrong_worktree,
        artifact,
    )
    .is_err());
    assert!(orch_host::wake::validate_panel_staging_lease_for_test(
        ".worktrees/../escaped",
        &exact,
        artifact,
    )
    .is_err());
}
