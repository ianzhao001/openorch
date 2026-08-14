mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn temp_root(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("target/test-tmp")
        .join(format!(
            "b114-session-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r1")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r1\n").unwrap();
    root
}

fn write_registry(root: &Path, mode: Option<&str>, session_id: &str) {
    let mode_line = mode
        .map(|value| format!("    mode: {value}\n"))
        .unwrap_or_default();
    let yaml = format!(
        "agents:\n  alpha:\n    injectable: true\n{mode_line}    sessionId: \"{session_id}\"\n    wake:\n      argv: [\"/bin/echo\", \"{{message}}\"]\n    pokeHint: \"\"\n"
    );
    fs::write(root.join("coordination/agents.yaml"), yaml).unwrap();
}

fn write_events(root: &Path, events: &[serde_json::Value]) {
    let text = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        root.join("coordination/rounds/r1/events.jsonl"),
        format!("{text}\n"),
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn activate_root_manual(root: &Path) {
    fs::create_dir_all(root.join("coordination/modes")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r1/tasks")).unwrap();
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
budgets: {round: {wallMinutes: 60, maxModelWakes: 10}}
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
        root.join("coordination/rounds/r1/tasks/B114.md"),
        "---\ntaskId: B114\nround: r1\nagent: executor-opencode\nseedProtocol: pure-spec\nwriteSet: [fixture.txt]\nfrozenPaths: [coordination/**]\ngates: {fast: [testFast]}\nbudgets: {wallMinutes: 10}\nrequiredReviews:\n  - {role: primary, agent: executor-claw}\nrequiredEvidence: [cli]\n---\nfixture\n",
    )
    .unwrap();
    orch_host::plan::run_plan(root).unwrap();
    orch_host::round::run_sign_off(root, Some("session CLI fixture")).unwrap();
}

#[test]
fn show_displays_configured_effective_overlay_and_legacy() {
    let root = temp_root("show");
    write_registry(&root, Some("resume"), "session-1");
    write_events(
        &root,
        &[json!({
            "eventId": "applied-1",
            "ts": "2026-07-26T00:00:00Z",
            "actor": "test",
            "type": "SessionOverlayApplied",
            "round": "r1",
            "payload": {
                "agent": "alpha",
                "fault": "thread-not-found",
                "faultEventId": "delivered-1",
                "generation": "lease-1"
            }
        })],
    );
    let output = run(&root, &["session", "show"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("configured=resume effective=fresh"),
        "{stdout}"
    );
    assert!(stdout.contains("overlay=thread-not-found"), "{stdout}");
    assert!(stdout.contains("generation=lease-1"), "{stdout}");
    assert!(stdout.contains("legacy=no"), "{stdout}");
}

#[test]
fn legacy_registry_is_compatible_and_marked_legacy() {
    let root = temp_root("legacy");
    write_registry(&root, None, "fresh-thread-per-wake");
    write_events(&root, &[]);
    let output = run(&root, &["session", "show"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("configured=fresh effective=fresh"),
        "{stdout}"
    );
    assert!(stdout.contains("overlay=none legacy=yes"), "{stdout}");
}

#[test]
fn set_is_rejected_in_root_manual_without_changing_registry_or_overlay() {
    let root = temp_root("set");
    write_registry(&root, Some("resume"), "old-session");
    let applied = json!({
        "eventId": "applied-1",
        "ts": "2026-07-26T00:00:00Z",
        "actor": "test",
        "type": "SessionOverlayApplied",
        "round": "r1",
        "payload": {
            "agent": "alpha",
            "fault": "context-exhausted",
            "faultEventId": "delivered-1",
            "generation": "lease-1"
        }
    });
    write_events(&root, &[applied]);
    activate_root_manual(&root);
    let registry_before = fs::read(root.join("coordination/agents.yaml")).unwrap();
    let ledger_before = fs::read(root.join("coordination/rounds/r1/events.jsonl")).unwrap();
    let set = run(
        &root,
        &["session", "set", "alpha", "new-session", "--mode", "resume"],
    );
    assert!(!set.status.success());
    assert!(String::from_utf8_lossy(&set.stderr).contains("root-manual"));
    assert_eq!(
        fs::read(root.join("coordination/agents.yaml")).unwrap(),
        registry_before
    );
    assert_eq!(
        fs::read(root.join("coordination/rounds/r1/events.jsonl")).unwrap(),
        ledger_before
    );

    let show = run(&root, &["session", "show"]);
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(
        stdout.contains("configured=resume effective=fresh"),
        "{stdout}"
    );
}
