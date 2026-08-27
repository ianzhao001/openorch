use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};
use orch_host::wake::{
    classify_review_panel_invalid_v1, evaluate_review_panel_v1, ReviewPanelDecisionV1,
    ReviewPanelPolicyV1, ReviewPanelSeatStateV1, ReviewPanelSeatV1,
};

fn fake_agy(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join("agy");
    fs::write(
        &path,
        r#"#!/bin/sh
message=''
previous=''
for argument in "$@"; do
  if [ "$previous" = '-p' ]; then message=$argument; break; fi
  previous=$argument
done
now=$(date +%s)
printf '%s|%s\n' "$now" "$message" >> "$AGY_CALLS"
if [ "${AGY_BAD-}" = '1' ]; then
  printf 'wrong-answer\n'
elif [ "${AGY_EMPTY_FORMAL-}" = '1' ] && ! printf '%s' "$message" | grep -q 'Authentication canary'; then
  :
else
  token=${message##*: }
  printf '%s\n' "$token"
fi
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn empty_formal_answer_is_system_invalid_after_one_formal_call_and_has_no_gen2() {
    let root = orch_host::util::test_scratch_dir("b310-agy-empty-formal");
    let calls = root.join("calls.log");
    let log = root.join("wake.log");
    fs::write(&log, b"").unwrap();
    let agy = fake_agy(&root);
    let formal = "FORMAL-EMPTY-ANSWER";
    let argv = vec![
        agy.display().to_string(),
        "-p".to_string(),
        formal.to_string(),
        "--model".to_string(),
        "gemini-3.7-flash-high".to_string(),
        "--effort".to_string(),
        "high".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    let env = BTreeMap::from([
        ("AGY_CALLS".to_string(), calls.display().to_string()),
        ("AGY_EMPTY_FORMAL".to_string(), "1".to_string()),
    ]);
    let error = orch_host::wake::run_agy_managed_preflight_for_test(
        &root,
        &argv,
        &env,
        &log,
        "11111111-2222-4333-8444-555555555555",
        formal,
        "antigravity",
        "gemini-3.7-flash-high",
        "high",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("empty final answer"), "{error}");
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 2);
    assert_eq!(
        classify_review_panel_invalid_v1(
            "executor-antigravity",
            "empty",
            "formal provider returned no final answer",
        ),
        ReviewPanelSeatStateV1::SystemInvalid
    );
    fs::remove_dir_all(root).ok();
}

#[test]
fn canary_is_exact_and_precedes_the_formal_call_by_five_seconds() {
    let root = orch_host::util::test_scratch_dir("b310-agy-preflight-order");
    let calls = root.join("calls.log");
    let log = root.join("wake.log");
    fs::write(&log, b"").unwrap();
    let agy = fake_agy(&root);
    let formal = "FORMAL-QUESTION-MUST-NOT-ENTER-CANARY";
    let argv = vec![
        agy.display().to_string(),
        "-p".to_string(),
        formal.to_string(),
        "--model".to_string(),
        "gemini-3.7-flash-high".to_string(),
        "--effort".to_string(),
        "high".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    let env = BTreeMap::from([("AGY_CALLS".to_string(), calls.display().to_string())]);
    let started = Instant::now();
    orch_host::wake::run_agy_managed_preflight_for_test(
        &root,
        &argv,
        &env,
        &log,
        "11111111-2222-4333-8444-555555555555",
        formal,
        "antigravity",
        "gemini-3.7-flash-high",
        "high",
    )
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(5));
    let observed = fs::read_to_string(&calls).unwrap();
    assert_eq!(observed.lines().count(), 2);
    assert!(observed.lines().next().unwrap().contains("Authentication canary"));
    let receipt = fs::read_to_string(&log).unwrap();
    assert!(receipt.contains("\"type\":\"agy.preflight\""));
    assert!(receipt.contains("\"model\":\"gemini-3.7-flash-high\""));

    let lines = fs::read_to_string(&calls)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert!(lines[1].contains(formal));
    let first_at = lines[0].split('|').next().unwrap().parse::<u64>().unwrap();
    let formal_at = lines[1].split('|').next().unwrap().parse::<u64>().unwrap();
    assert!(formal_at.saturating_sub(first_at) >= 5);
    fs::remove_dir_all(root).ok();
}

#[test]
fn wrong_canary_never_reaches_the_formal_request() {
    let root = orch_host::util::test_scratch_dir("b310-agy-preflight-reject");
    let calls = root.join("calls.log");
    let log = root.join("wake.log");
    fs::write(&log, b"").unwrap();
    let agy = fake_agy(&root);
    let formal = "FORMAL-MUST-STAY-UNSENT";
    let argv = vec![
        agy.display().to_string(),
        "-p".to_string(),
        formal.to_string(),
        "--model".to_string(),
        "gemini-3.7-flash-high".to_string(),
        "--effort".to_string(),
        "high".to_string(),
        "--dangerously-skip-permissions".to_string(),
    ];
    let env = BTreeMap::from([
        ("AGY_CALLS".to_string(), calls.display().to_string()),
        ("AGY_BAD".to_string(), "1".to_string()),
    ]);
    let error = orch_host::wake::run_agy_managed_preflight_for_test(
        &root,
        &argv,
        &env,
        &log,
        "11111111-2222-4333-8444-555555555555",
        formal,
        "antigravity",
        "gemini-3.7-flash-high",
        "high",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("exact echo"), "{error}");
    let calls = fs::read_to_string(&calls).unwrap();
    assert_eq!(calls.lines().count(), 1);
    assert!(!calls.contains(formal));
    fs::remove_dir_all(root).ok();
}

#[test]
fn agy_empty_or_auth_failure_is_system_invalid_and_never_requests_gen2() {
    for (state, reason) in [
        ("empty", "provider returned no final answer"),
        ("failed", "authentication preflight failed"),
    ] {
        assert_eq!(
            classify_review_panel_invalid_v1("executor-antigravity", state, reason),
            ReviewPanelSeatStateV1::SystemInvalid
        );
    }
    let seats = vec![
        ReviewPanelSeatV1 {
            seat_id: "agy".to_string(),
            generation: 1,
            role: "nongate".to_string(),
            agent: "executor-antigravity".to_string(),
            primary_lineage: false,
            retry_eligible: false,
            state: ReviewPanelSeatStateV1::SystemInvalid,
        },
        ReviewPanelSeatV1 {
            seat_id: "oc".to_string(),
            generation: 1,
            role: "primary".to_string(),
            agent: "executor-opencode".to_string(),
            primary_lineage: true,
            retry_eligible: true,
            state: ReviewPanelSeatStateV1::Pending,
        },
        ReviewPanelSeatV1 {
            seat_id: "pi".to_string(),
            generation: 1,
            role: "secondary".to_string(),
            agent: "executor-pi".to_string(),
            primary_lineage: false,
            retry_eligible: true,
            state: ReviewPanelSeatStateV1::Pending,
        },
    ];
    let decision = evaluate_review_panel_v1(
        &ReviewPanelPolicyV1 {
            minimum_passes: 2,
            require_primary_pass: true,
            maximum_business_retries: 1,
            nongate_substitutes_secondary_only: true,
        },
        &seats,
        0,
        0,
    );
    assert_eq!(decision, ReviewPanelDecisionV1::Awaiting);
}
