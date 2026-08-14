mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("target/test-tmp")
        .join(format!(
            "b130-verdict-cli-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn signed_collected_root() -> (PathBuf, PathBuf) {
    let root = root();
    let sentinel = root.join("PROVIDER-SPAWNED");
    for path in [
        "coordination/runtime",
        "coordination/modes",
        "coordination/rounds/r1/tasks",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r1\n").unwrap();
    fs::write(
        root.join("coordination/modes/test.yaml"),
        r#"agents:
  executor: {adapter: test, tier: none}
  verifier: {adapter: root-manual, tier: none}
hitl: {mergeGate: auto}
verification: {mode: root-manual-fixed-head}
liveness: {monitorSeconds: 15, workingStallMinutes: 10, confirmSamples: 2}
scheduling:
  allowedAgents: [executor-desktop, executor-claw, executor-opencode]
  capacities:
    executor-desktop: {agent: 2, quota: 2, roles: [implement]}
    executor-claw: {agent: 1, quota: 1, roles: [primary-review]}
    executor-opencode: {agent: 3, quota: 3, roles: [implement, secondary-review]}
budgets: {round: {wallMinutes: 60, maxModelWakes: 20}}
git: {pushPolicy: forbidden, mergePolicy: ff-only-else-no-ff}
"#,
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "scope: {protectedPaths: [\"coordination/**\"]}\ngit: {pushPolicy: forbidden}\ncommands:\n  testFast: {argv: [\"sh\", \"-c\", \"exit 0\"], timeoutSeconds: 30}\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/rounds/r1/tasks/B130.md"),
        "---\ntaskId: B130\nround: r1\nagent: executor-opencode\nseedProtocol: pure-spec\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {fast: [testFast]}\nbudgets: {wallMinutes: 10}\nrequiredReviews:\n  - {role: primary, agent: executor-claw}\nrequiredEvidence: [cli]\n---\nfixture\n",
    )
    .unwrap();
    let sentinel_command = format!("touch {}", sentinel.display());
    let agents = serde_json::json!({
        "agents": {
            "executor-opencode": {
                "injectable": true,
                "sessionId": "executor-session",
                "wake": {"argv": ["/bin/sh", "-c", sentinel_command]}
            },
            "planner": {
                "injectable": true,
                "sessionId": "",
                "wake": {
                    "argv": [],
                    "freshArgv": ["/bin/sh", "-c", format!("touch {}", sentinel.display())]
                }
            }
        }
    });
    fs::write(
        root.join("coordination/agents.yaml"),
        serde_json::to_vec_pretty(&agents).unwrap(),
    )
    .unwrap();
    orch_host::plan::run_plan(&root).unwrap();
    orch_host::round::run_sign_off(&root, Some("entrypoint fixture")).unwrap();
    let receipt = orch_host::ledger::event(
        "CollectGateSuccessReceipt",
        "runtime:orch",
        Some("B130"),
        Some("r1"),
        serde_json::json!({
            "actionId": "collect-B130-A0001",
            "attemptId": "B130-A0001",
            "attemptNo": 1,
            "agent": "executor-opencode",
            "baseSha": "base",
            "goPath": "coordination/rounds/r1/dispatch/executor-opencode/GO-B130-A0001.md",
            "branchSha": "head"
        }),
    );
    let dispatch = orch_host::ledger::event(
        "DispatchIssued",
        "runtime:orch",
        Some("B130"),
        Some("r1"),
        serde_json::json!({
            "agent": "executor-opencode",
            "attemptId": "B130-A0001",
            "attemptNo": 1,
            "baseSha": "base",
            "goPath": "coordination/rounds/r1/dispatch/executor-opencode/GO-B130-A0001.md"
        }),
    );
    let observed = orch_host::ledger::event(
        "ReportObserved",
        "runtime:orch",
        Some("B130"),
        Some("r1"),
        serde_json::json!({"attemptId": "B130-A0001"}),
    );
    let collect = orch_host::ledger::event(
        "ReportCollectCompleted",
        "runtime:orch",
        Some("B130"),
        Some("r1"),
        serde_json::json!({
            "actionId": "collect-B130-A0001",
            "attemptId": "B130-A0001",
            "attemptNo": 1,
            "agent": "executor-opencode",
            "baseSha": "base",
            "goPath": "coordination/rounds/r1/dispatch/executor-opencode/GO-B130-A0001.md",
            "branchSha": "head",
            "gateReceipt": receipt.event_id
        }),
    );
    orch_host::ledger::append(&root, "r1", &[dispatch, observed, receipt, collect]).unwrap();
    (root, sentinel)
}

fn event_kinds(root: &Path) -> Vec<String> {
    orch_core::read_ledger(&root.join("coordination/rounds/r1/events.jsonl"))
        .unwrap()
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

#[test]
fn verdict_surface_requires_fixed_tuple_and_rejects_short_sha_nonzero() {
    let root = root();
    let output = fixture_orch_command(&[])
        .args([
            "--root",
            root.to_str().unwrap(),
            "verdict",
            "B130",
            "--attempt",
            "B130-A0001",
            "--expected-head",
            "abc1234",
            "--expected-main",
            "0000000000000000000000000000000000000000",
            "--verdict",
            "pass",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("40"));
}

#[test]
fn run_wave_model_override_is_removed() {
    let root = root();
    let output = fixture_orch_command(&[])
        .args([
            "--root",
            root.to_str().unwrap(),
            "run-wave",
            "--model",
            "sonnet",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));

    let verify = fixture_orch_command(&[])
        .args([
            "--root",
            root.to_str().unwrap(),
            "verify",
            "B130",
            "--model",
            "sonnet",
        ])
        .output()
        .unwrap();
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stderr).contains("unexpected argument"));
}

#[test]
fn verdict_attempt_and_reason_are_rejected_syntax_first_without_filesystem_writes() {
    let full = "0".repeat(40);
    for args in [
        vec![
            "verdict",
            "B130",
            "--attempt",
            "../A1",
            "--expected-head",
            &full,
            "--expected-main",
            &full,
            "--verdict",
            "pass",
        ],
        vec![
            "verdict",
            "B130",
            "--attempt",
            "B130-A0001",
            "--expected-head",
            &full,
            "--expected-main",
            &full,
            "--verdict",
            "fail",
            "--reason",
            "   ",
        ],
        vec![
            "verdict",
            "B130",
            "--attempt",
            "B130-A0001",
            "--expected-head",
            &full,
            "--expected-main",
            &full,
            "--verdict",
            "pass",
            "--reason",
            "not allowed",
        ],
    ] {
        let root = root();
        let before = fs::read_dir(&root).unwrap().count();
        let output = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(fs::read_dir(&root).unwrap().count(), before);
    }
}

#[test]
fn root_manual_step_serve_and_wave_never_spawn_external_provider() {
    for (label, args, expected_code) in [
        ("step", vec!["step"], Some(0)),
        ("serve", vec!["serve", "--once"], Some(0)),
        ("wave", vec!["run-wave", "--timeout-secs", "1"], Some(7)),
    ] {
        let (root, sentinel) = signed_collected_root();
        orch_host::inbox::add(&root, &format!("tempt planner from {label}"), 1).unwrap();
        let output = fixture_orch_command(&[])
            .arg("--root")
            .arg(&root)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            expected_code,
            "{label}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!sentinel.exists(), "{label} spawned external provider");
        let kinds = event_kinds(&root);
        for forbidden in [
            "VerifyStarted",
            "PlannerTurnStarted",
            "WakeIssued",
            "WaveBlockerRecorded",
            "MergeStarted",
        ] {
            assert!(
                !kinds.iter().any(|kind| kind == forbidden),
                "{label}: {kinds:?}"
            );
        }
    }
}
