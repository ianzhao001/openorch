use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use orch_host::channel::{
    capture_attachment_manifest_v1, preflight_invocation_v1, prepare_invocation,
    render_invocation_v1, run_preflighted_invocation_v1, InvocationAction, InvocationContextV1,
    InvocationRequest,
};
use orch_host::harness::{DriverAction, HarnessId};
use orch_host::harness_config::{
    parse_harness_config_snapshot, HarnessAction, HarnessAvailability,
};

struct Fixture {
    root: PathBuf,
    executable: PathBuf,
    head: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::current_dir()
            .unwrap()
            .join(".cowork-temp")
            .join(format!("B320-driver-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Orch Test",
                "-c",
                "user.email=orch@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        );
        let executable = root.join("fake-provider.sh");
        fs::write(
            &executable,
            b"#!/bin/sh\nprintf 'cwd=%s\\n' \"$PWD\"\nprintf 'ambient=%s\\n' \"${ORCH_FAKE_PIN-unset}\"\nprintf 'args=%s\\n' \"$*\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let head = git_output(&root, &["rev-parse", "HEAD"]);
        Self {
            root: fs::canonicalize(root).unwrap(),
            executable,
            head,
        }
    }

    fn snapshot(&self, model: &str) -> orch_host::harness_config::HarnessConfigSnapshot {
        let yaml = format!(
            "version: 1\nharnesses:\n  alpha:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: {model}, effort: high}}\n    consult: {{mode: pure}}\n    cwdPolicy: project-root\n",
            self.executable.display()
        );
        parse_harness_config_snapshot(&self.root.join(".orch/harnesses.yaml"), &yaml).unwrap()
    }

    fn request(&self) -> InvocationRequest {
        InvocationRequest {
            alias: "alpha".into(),
            action: InvocationAction::Consult,
            prompt: "answer exactly".into(),
            project_root: self.root.clone(),
            target_worktree: PathBuf::new(),
            target_head: self.head.clone(),
            attachments: capture_attachment_manifest_v1(&[]).unwrap(),
        }
    }

    fn context(&self) -> InvocationContextV1 {
        InvocationContextV1 {
            action_id: "consult-action-1".into(),
            wake_id: "consult-member-1".into(),
            round: "r83".into(),
            task_id: "CONSULT".into(),
            attempt_id: "CONSULT-A0000".into(),
            review_output: None,
            orch_executable: self.executable.clone(),
            deadline_secs: 60,
        }
    }
}

impl Drop for Fixture {
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
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn render_preflight_and_spawn_share_closed_command_facts() {
    let fixture = Fixture::new("spawn");
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let request_digest = prepared.request_digest().to_string();
    let rendered = render_invocation_v1(prepared, fixture.context()).unwrap();
    assert!(rendered.argv().iter().any(|value| value == "local/model-a"));
    assert!(rendered.argv().iter().any(|value| value == "high"));
    assert!(rendered.argv().iter().any(|value| value == "--pure"));
    assert!(!rendered
        .env()
        .keys()
        .any(|key| key.starts_with("ORCH_FAKE")));
    assert_eq!(rendered.prepared().request_digest(), request_digest);

    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    assert!(output.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&format!("cwd={}", fixture.root.display())));
    assert!(stdout.contains("ambient=unset"));
    assert!(stdout.contains("--model local/model-a"));
    assert!(stdout.contains("--format json"));
}

#[test]
fn executable_and_head_drift_fail_before_spawn() {
    let executable_fixture = Fixture::new("exe-drift");
    let prepared = prepare_invocation(
        &executable_fixture.snapshot("model-a"),
        executable_fixture.request(),
    )
    .unwrap();
    let rendered = render_invocation_v1(prepared, executable_fixture.context()).unwrap();
    fs::write(
        &executable_fixture.executable,
        b"#!/bin/sh\nexit 0\n# replaced\n",
    )
    .unwrap();
    assert!(preflight_invocation_v1(rendered)
        .unwrap_err()
        .to_string()
        .contains("identity"));

    let head_fixture = Fixture::new("head-drift");
    let prepared =
        prepare_invocation(&head_fixture.snapshot("model-a"), head_fixture.request()).unwrap();
    let rendered = render_invocation_v1(prepared, head_fixture.context()).unwrap();
    fs::write(head_fixture.root.join("next.txt"), b"next\n").unwrap();
    git(&head_fixture.root, &["add", "next.txt"]);
    git(
        &head_fixture.root,
        &[
            "-c",
            "user.name=Orch Test",
            "-c",
            "user.email=orch@example.invalid",
            "commit",
            "-qm",
            "next",
        ],
    );
    assert!(preflight_invocation_v1(rendered)
        .unwrap_err()
        .to_string()
        .contains("HEAD"));
}

#[test]
fn tuple_and_config_changes_produce_a_new_request_identity() {
    let fixture = Fixture::new("tuple-digest");
    let first = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let second = prepare_invocation(&fixture.snapshot("model-b"), fixture.request()).unwrap();
    assert_ne!(first.config_digest(), second.config_digest());
    assert_ne!(first.request_digest(), second.request_digest());
}

#[test]
fn consult_requires_fixed_head_and_attachment_spelling_is_canonical() {
    let fixture = Fixture::new("lexical");
    let mut request = fixture.request();
    request.target_head.clear();
    assert!(prepare_invocation(&fixture.snapshot("model-a"), request).is_err());

    let attachment = fixture.root.join("attachment.md");
    fs::write(&attachment, b"attached").unwrap();
    let dotted = PathBuf::from(format!("{}/./attachment.md", fixture.root.display()));
    let repeated = PathBuf::from(format!("{}//attachment.md", fixture.root.display()));
    assert!(capture_attachment_manifest_v1(&[dotted.as_path()]).is_err());
    assert!(capture_attachment_manifest_v1(&[repeated.as_path()]).is_err());
}

#[test]
fn driver_catalog_is_the_closed_action_support_truth() {
    for driver in HarnessId::ALL {
        for action in [
            DriverAction::Execute,
            DriverAction::Review,
            DriverAction::Consult,
        ] {
            assert_eq!(
                driver.supports_action(action),
                driver.driver_contract(action).is_some(),
                "{} {}",
                driver.as_str(),
                action.as_str()
            );
        }
    }
    assert!(!HarnessId::Cursor.supports_action(DriverAction::Review));
    assert!(HarnessId::SmartClaw.supports_action(DriverAction::Review));
    assert!(!HarnessId::Agy.supports_action(DriverAction::Consult));
    assert!(!HarnessId::Dclaw.supports_action(DriverAction::Consult));
    assert!(!HarnessId::Pi.supports_action(DriverAction::Consult));
    assert!(!HarnessId::ZCode.supports_action(DriverAction::Consult));
    assert!(!HarnessId::Dsh.supports_action(DriverAction::Consult));
}

#[test]
fn unsupported_action_override_is_row_local_while_driver_without_actions_is_unavailable() {
    let fixture = Fixture::new("row-local-action");
    let yaml = format!(
        "version: 1\nharnesses:\n  alpha:\n    driver: dsh\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    consult: {{mode: headless}}\n    cwdPolicy: project-root\n  beta:\n    driver: dclaw\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",
        fixture.executable.display(),
        fixture.executable.display()
    );
    let snapshot =
        parse_harness_config_snapshot(&fixture.root.join(".orch/harnesses.yaml"), &yaml).unwrap();
    let rows = snapshot.discover_without_tokens();
    assert!(matches!(
        rows[0].availability(),
        &HarnessAvailability::Supported
    ));
    assert!(matches!(
        rows[1].availability(),
        &HarnessAvailability::Unsupported(_)
    ));
    snapshot.resolve("alpha", HarnessAction::Review).unwrap();
    assert!(snapshot.resolve("alpha", HarnessAction::Consult).is_err());
}

#[test]
fn smartclaw_code_owned_wrapper_is_authenticated_without_an_execute_bit() {
    let fixture = Fixture::new("smartclaw-wrapper");
    // This case exercises the retained selfhost wrapper path. Standalone uses installed assets.
    fs::create_dir_all(fixture.root.join("coordination")).unwrap();
    fs::write(fixture.root.join("coordination/PROJECT-BINDING.yaml"), "fixture selfhost marker\n").unwrap();
    let wrapper = fixture.root.join("coordination/scripts/wake-multica.sh");
    fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    fs::write(&wrapper, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o644)).unwrap();
    let yaml = format!(
        "version: 1\nharnesses:\n  alpha:\n    driver: smartclaw\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",
        fixture.executable.display()
    );
    let snapshot =
        parse_harness_config_snapshot(&fixture.root.join(".orch/harnesses.yaml"), &yaml).unwrap();
    let prepared = prepare_invocation(&snapshot, fixture.request()).unwrap();
    let rendered = render_invocation_v1(prepared, fixture.context()).unwrap();
    assert_eq!(rendered.argv()[0], "/bin/sh");
    assert_eq!(Path::new(&rendered.argv()[1]), wrapper);
    preflight_invocation_v1(rendered).unwrap();
}

#[test]
fn agy_native_print_wait_receives_the_effective_invocation_deadline_across_aliases() {
    let fixture = Fixture::new("agy-native-wait");
    for (alias, model, deadline, action) in [
        ("alpha", "model-a", 7, InvocationAction::Execute),
        ("renamed", "model-b", 90, InvocationAction::Review),
    ] {
        let yaml = format!("version: 1\nharnesses:\n  {alias}:\n    driver: agy\n    executable: {}\n    enabled: true\n    defaults: {{provider: antigravity, model: {model}, effort: high}}\n    cwdPolicy: project-root\n", fixture.executable.display());
        let snapshot = parse_harness_config_snapshot(&fixture.root.join(".orch/harnesses.yaml"), &yaml).unwrap();
        let mut request = fixture.request();
        request.alias = alias.into(); request.action = action;
        request.target_worktree = fixture.root.clone();
        let mut context = fixture.context(); context.deadline_secs = deadline;
        if action == InvocationAction::Review { context.review_output = Some(fixture.root.join("review.md")); }
        let rendered = render_invocation_v1(prepare_invocation(&snapshot, request).unwrap(), context).unwrap();
        let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
        assert!(output.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&format!("--print-timeout {deadline}s")),
            "native CLI would still use its own default wait: {stdout}");
        assert!(stdout.contains(&format!("--model {model}")));
        assert_eq!(output.hard_deadline_secs, deadline);
    }
}

#[test]
fn dsh_project_root_requires_an_external_non_temporary_orch_runtime_target() {
    let fixture = Fixture::new("dsh-runtime-target");
    let wrapper = fixture.root.join("orch/scripts/wake-dsh-stream.sh");
    fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    fs::write(&wrapper, b"#!/bin/sh\nexit 0\n").unwrap();
    let yaml = format!(
        "version: 1\nharnesses:\n  dsh-review:\n    driver: dsh\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: model-a, effort: high}}\n    review: {{mode: minimal}}\n    cwdPolicy: project-root\n",
        fixture.executable.display()
    );
    let snapshot =
        parse_harness_config_snapshot(&fixture.root.join(".orch/harnesses.yaml"), &yaml).unwrap();
    let request = InvocationRequest {
        alias: "dsh-review".into(),
        action: InvocationAction::Review,
        prompt: "review the fixed head".into(),
        project_root: fixture.root.clone(),
        target_worktree: fixture.root.clone(),
        target_head: fixture.head.clone(),
        attachments: capture_attachment_manifest_v1(&[]).unwrap(),
    };
    let context = |orch_executable: PathBuf| InvocationContextV1 {
        action_id: "dsh-review-action".into(),
        wake_id: "019fc320-1111-4222-8333-444455556666".into(),
        round: "r83".into(),
        task_id: "B320".into(),
        attempt_id: "B320-A0002".into(),
        review_output: Some(fixture.root.join("review.md")),
        orch_executable,
        deadline_secs: 60,
    };

    let internal_orch = fixture.root.join("orch/target/debug/orch");
    fs::create_dir_all(internal_orch.parent().unwrap()).unwrap();
    fs::write(
        fixture.root.join("orch/target/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(&internal_orch, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&internal_orch).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&internal_orch, permissions).unwrap();
    let internal = render_invocation_v1(
        prepare_invocation(&snapshot, request.clone()).unwrap(),
        context(internal_orch.clone()),
    )
    .unwrap();
    let error = preflight_invocation_v1(internal).unwrap_err().to_string();
    assert!(error.contains("仓外、非临时 target"), "{error}");

    let external_target = fixture
        .root
        .parent()
        .unwrap()
        .join(format!("B320-external-dsh-target-{}", std::process::id()));
    let _ = fs::remove_dir_all(&external_target);
    let external_orch = external_target.join("debug/orch");
    fs::create_dir_all(external_orch.parent().unwrap()).unwrap();
    fs::write(
        external_target.join("CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(&external_orch, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&external_orch).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&external_orch, permissions).unwrap();
    let external = render_invocation_v1(
        prepare_invocation(&snapshot, request.clone()).unwrap(),
        context(external_orch.clone()),
    )
    .unwrap();
    assert_eq!(
        external.env().get("ORCH_DSH_PRESET").map(String::as_str),
        Some("minimal")
    );
    preflight_invocation_v1(external).unwrap();

    let untagged_target = fixture.root.parent().unwrap().join(format!(
        "B320-untagged-dsh-target-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&untagged_target);
    let untagged_orch = untagged_target.join("debug/orch");
    fs::create_dir_all(untagged_orch.parent().unwrap()).unwrap();
    fs::write(&untagged_orch, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&untagged_orch).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&untagged_orch, permissions).unwrap();
    let untagged = render_invocation_v1(
        prepare_invocation(&snapshot, request.clone()).unwrap(),
        context(untagged_orch),
    )
    .unwrap();
    let error = preflight_invocation_v1(untagged)
        .unwrap_err()
        .to_string();
    assert!(error.contains("CACHEDIR.TAG"), "{error}");
    fs::remove_dir_all(untagged_target).unwrap();

    let mut target_permissions = fs::metadata(&external_target).unwrap().permissions();
    target_permissions.set_mode(0o555);
    fs::set_permissions(&external_target, target_permissions).unwrap();
    let unwritable = render_invocation_v1(
        prepare_invocation(&snapshot, request.clone()).unwrap(),
        context(external_orch.clone()),
    )
    .unwrap();
    let unwritable_result = preflight_invocation_v1(unwritable);
    let mut target_permissions = fs::metadata(&external_target).unwrap().permissions();
    target_permissions.set_mode(0o755);
    fs::set_permissions(&external_target, target_permissions).unwrap();
    let error = unwritable_result.unwrap_err().to_string();
    assert!(error.contains("不可写"), "{error}");

    let global_orch = external_target.join("bin/orch");
    fs::create_dir_all(global_orch.parent().unwrap()).unwrap();
    fs::write(&global_orch, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&global_orch).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&global_orch, permissions).unwrap();
    let global = render_invocation_v1(
        prepare_invocation(&snapshot, request.clone()).unwrap(),
        context(global_orch),
    )
    .unwrap();
    let error = preflight_invocation_v1(global).unwrap_err().to_string();
    assert!(
        error.contains("Cargo debug/release target profile"),
        "{error}"
    );

    let manual = render_invocation_v1(
        prepare_invocation(&snapshot, request.clone()).unwrap(),
        InvocationContextV1 {
            action_id: "manual-dsh-review".into(),
            wake_id: "019fc320-1111-4222-8333-444455556667".into(),
            round: "r83".into(),
            task_id: "MANUAL".into(),
            attempt_id: "MANUAL-A0000".into(),
            review_output: Some(fixture.root.join("manual-review.md")),
            orch_executable: internal_orch,
            deadline_secs: 60,
        },
    )
    .unwrap();
    preflight_invocation_v1(manual).unwrap();

    let temporary_target = std::env::temp_dir().join(format!(
        "B320-manual-dsh-target-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&temporary_target);
    let temporary_orch = temporary_target.join("debug/orch");
    fs::create_dir_all(temporary_orch.parent().unwrap()).unwrap();
    fs::write(
        temporary_target.join("CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .unwrap();
    fs::write(&temporary_orch, b"#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = fs::metadata(&temporary_orch).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&temporary_orch, permissions).unwrap();
    let temporary_manual = render_invocation_v1(
        prepare_invocation(&snapshot, request).unwrap(),
        InvocationContextV1 {
            action_id: "manual-temporary-dsh-review".into(),
            wake_id: "019fc320-1111-4222-8333-444455556668".into(),
            round: "r83".into(),
            task_id: "MANUAL".into(),
            attempt_id: "MANUAL-A0000".into(),
            review_output: Some(fixture.root.join("manual-temporary-review.md")),
            orch_executable: temporary_orch,
            deadline_secs: 60,
        },
    )
    .unwrap();
    let error = preflight_invocation_v1(temporary_manual)
        .unwrap_err()
        .to_string();
    assert!(error.contains("/tmp/TMPDIR"), "{error}");
    fs::remove_dir_all(temporary_target).unwrap();
    fs::remove_dir_all(external_target).unwrap();
}

#[test]
fn synchronous_driver_deadline_returns_a_typed_timeout_without_hanging() {
    use std::os::unix::process::ExitStatusExt;
    let fixture = Fixture::new("deadline");
    fs::write(&fixture.executable, b"#!/bin/sh\n/bin/sleep 5\n").unwrap();
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let mut context = fixture.context();
    context.deadline_secs = 1;
    let rendered = render_invocation_v1(prepared, context).unwrap();
    let preflighted = preflight_invocation_v1(rendered).unwrap();
    let started = Instant::now();
    let output = run_preflighted_invocation_v1(preflighted).unwrap();
    assert_eq!(output.reason, orch_host::channel::ChannelExitReason::HardDeadline);
    assert_eq!(output.hard_deadline_secs, 1);
    assert_eq!(output.status.expect("owned child must actually be reaped").signal(), Some(9));
    assert_eq!(output.exit_code(), None);
    assert!(!output.success());
    assert!(output.process_group_terminated);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(output.facts()["upstreamTermination"], "unconfirmed",
        "actual local SIGKILL/reap must not attest provider-side cancellation");
}

#[test]
fn numeric_exit_72_is_an_observed_exit_not_a_runtime_deadline() {
    let fixture = Fixture::new("real-exit-72");
    fs::write(&fixture.executable, b"#!/bin/sh\nexit 72\n").unwrap();
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let rendered = render_invocation_v1(prepared, fixture.context()).unwrap();
    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    assert_eq!(output.reason, orch_host::channel::ChannelExitReason::Exited);
    assert_eq!(output.exit_code(), Some(72));
    assert!(output.process_group_terminated);
    assert_eq!(orch_host::channel::classify_driver_failure(
        HarnessId::OpenCode, "", "", output.reason, output.exit_code(),
    ).unwrap().class_name(), "unknown");
}

#[test]
fn legitimate_silent_work_finishes_without_an_idle_cancellation() {
    let fixture = Fixture::new("legitimate-silence");
    fs::write(&fixture.executable,
        b"#!/bin/sh\n/bin/sleep 1.2\nprintf 'complete after legal tool silence\\n'\n").unwrap();
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let mut context = fixture.context(); context.deadline_secs = 10;
    let rendered = render_invocation_v1(prepared, context).unwrap();
    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    assert!(output.success());
    assert_eq!(output.reason, orch_host::channel::ChannelExitReason::Exited);
    assert!(output.elapsed_millis >= 1_100);
    assert!(output.first_frame_after_millis.is_some_and(|millis| millis >= 1_100));
    assert_eq!(output.stdout, b"complete after legal tool silence\n");
    assert!(output.process_group_terminated);
    assert!(output.observation_errors.is_empty());
}

#[test]
fn reaped_client_with_a_live_group_stays_unclosed_without_signaling_a_reused_leader() {
    let fixture = Fixture::new("unclosed-group");
    fs::write(&fixture.executable, b"#!/bin/sh\n(while [ ! -f release ]; do /bin/sleep 0.02; done; printf 'native work finished\\n' > native-finished) &\nprintf '%s\\n' $! > owned-child.pid\nexit 0\n").unwrap();
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    // Leave startup/reaping headroom under the full concurrent workspace gate;
    // the behavior under test is a reaped leader, not a one-second launch race.
    let mut context = fixture.context(); context.deadline_secs = 10;
    let rendered = render_invocation_v1(prepared, context).unwrap();
    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    let was_still_running = !fixture.root.join("native-finished").exists();
    // Release the exact owned fixture before assertions, including on regression.
    fs::write(fixture.root.join("release"), b"release\n").unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !fixture.root.join("native-finished").exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe extern "C" { fn killpg(pgid: i32, signal: i32) -> i32; }
    let group_ended = loop {
        // Signal zero only observes the fixture group; ESRCH is the sole proof.
        if unsafe { killpg(output.process_id as i32, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(3) { break true; }
        if Instant::now() >= until { break false; }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(was_still_running && fixture.root.join("native-finished").exists(),
        "fixture did not survive to release: facts={} stderr={}", output.facts(), String::from_utf8_lossy(&output.stderr));
    assert!(group_ended, "owned fixture group must end before its files are reclaimed");
    assert_eq!(output.exit_code(), Some(0));
    assert!(!output.process_group_terminated);
    assert!(output.observation_errors.iter().any(|error| error.contains("without current leader ownership proof")));
    assert!(output.stdout_capture_path.as_ref().is_some_and(|path| path.is_file()));
}

#[test]
fn synchronous_driver_waits_for_background_group_after_leader_exit() {
    let fixture = Fixture::new("background-group");
    let marker = fixture.root.join("escaped-marker");
    fs::write(
        &fixture.executable,
        format!(
            "#!/bin/sh\n(/bin/sleep 1; /usr/bin/touch {}) &\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let rendered = render_invocation_v1(prepared, fixture.context()).unwrap();
    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    assert!(output.success());
    assert!(output.process_group_terminated);
    assert!(output.elapsed_millis >= 1_000);
    assert!(marker.exists(), "the still-running owned child must finish before the channel returns");
    let completed = fs::metadata(&marker).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::metadata(&marker).unwrap().modified().unwrap(), completed);
}

#[test]
fn synchronous_driver_reads_original_capture_handles_after_path_replacement() {
    let fixture = Fixture::new("capture-replacement");
    fs::write(
        &fixture.executable,
        b"#!/bin/sh\nfor path in \"$PWD\"/.cowork-temp/channel-capture/*; do /bin/rm -f \"$path\"; /usr/bin/mkfifo \"$path\"; done\nprintf 'capture-handle-ok\\n'\n",
    )
    .unwrap();
    let prepared = prepare_invocation(&fixture.snapshot("model-a"), fixture.request()).unwrap();
    let rendered = render_invocation_v1(prepared, fixture.context()).unwrap();
    let output = run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap();
    assert!(output.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "capture-handle-ok\n"
    );
    for path in [output.stdout_capture_path.as_ref().unwrap(), output.stderr_capture_path.as_ref().unwrap()] {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o077, 0);
    }
    assert_eq!(fs::read(output.stdout_capture_path.as_ref().unwrap()).unwrap(), b"capture-handle-ok\n");
}

#[test]
fn standalone_wrapper_resolution_ignores_project_lookalikes() {
    let fixture = Fixture::new("standalone-wrapper-lookalike");
    let lookalike = fixture.root.join("coordination/scripts/wake-multica.sh");
    fs::create_dir_all(lookalike.parent().unwrap()).unwrap();
    fs::write(&lookalike, b"#!/bin/sh\ntouch untrusted-wrapper-started\n").unwrap();
    let yaml = format!("version: 1\nharnesses:\n  alpha:\n    driver: smartclaw\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n", fixture.executable.display());
    let snapshot = parse_harness_config_snapshot(&fixture.root.join(".orch/harnesses.yaml"), &yaml).unwrap();
    let rendered = render_invocation_v1(prepare_invocation(&snapshot, fixture.request()).unwrap(), fixture.context()).unwrap();
    let selected = Path::new(&rendered.argv()[1]);
    assert_ne!(selected, lookalike);
    assert!(!selected.starts_with(&fixture.root));
    assert_eq!(fs::read(selected).unwrap(), include_bytes!("../../../../coordination/scripts/wake-multica.sh"));
    preflight_invocation_v1(rendered).unwrap();
    assert!(!fixture.root.join("untrusted-wrapper-started").exists());
}
