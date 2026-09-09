//! B322 · explicit-member lightweight consultation contract.
//!
//! Expected red: compile (`E0432`) until the preset/judge surface is replaced.
//! M1 accept exit-zero without final text -> `exit_zero_without_final_text_is_invalid` red.
//! M2 accept tool-only output -> `tool_only_output_is_invalid` red.
//! M3 accept raw transcript as final -> `raw_transcript_is_invalid` red.
//! M4 trust an absent/nonterminal record -> `missing_or_untrusted_terminal_is_invalid` red.
//! M5 synthesize preset/judge/roster membership -> `explicit_members_are_nonempty_unique` or CLI companion red.
//! M6 erase valid siblings when one member fails -> `invalid_member_does_not_erase_valid_siblings` companion red.
//! M7 omit/mismatch ordered attachment digest -> `member_manifest_binds_the_ordered_attachment_snapshot` red.

use orch_host::consult::{
    classify_consult_answer_v3, ConsultAnswerInputV3, ConsultAnswerValidityV3,
    ConsultMemberManifestV3, ExplicitConsultMembers, LIGHTWEIGHT_CONSULT_CONTRACT_V1,
};
use orch_host::harness::{
    classify_terminal_observation, CapabilitySource, TerminalObservation, TerminalRecord,
};

fn terminal(final_text: Option<&str>) -> TerminalRecord {
    classify_terminal_observation(TerminalObservation {
        capability: CapabilitySource::Derived,
        exit_code: Some(0),
        exact_reason: "seed-fixture-final-drain".into(),
        turn_ended: true,
        final_text: final_text.map(str::to_string),
        final_text_sha256: None,
        output_path: None,
        output_sha256: None,
        usage: None,
        usage_absent_reason: Some("seed fixture does not report usage".into()),
        managed_scope_terminated: true,
        activity_seen: true,
        authenticated_cancel: false,
    })
    .expect("provider-neutral terminal record")
}

fn absent_terminal() -> TerminalRecord {
    classify_terminal_observation(TerminalObservation {
        capability: CapabilitySource::Absent,
        exit_code: None,
        exact_reason: "seed-fixture-no-terminal-source".into(),
        turn_ended: false,
        final_text: None,
        final_text_sha256: None,
        output_path: None,
        output_sha256: None,
        usage: None,
        usage_absent_reason: None,
        managed_scope_terminated: false,
        activity_seen: false,
        authenticated_cancel: false,
    })
    .expect("absent terminal record")
}

#[test]
fn repeated_harness_flags_preserve_explicit_order_without_a_preset() {
    assert_eq!(LIGHTWEIGHT_CONSULT_CONTRACT_V1, 1);
    let members =
        ExplicitConsultMembers::new(["alpha", "beta", "gamma"]).expect("explicit unique aliases");
    assert_eq!(members.as_slice(), ["alpha", "beta", "gamma"]);
}

#[test]
fn explicit_members_are_nonempty_unique() {
    assert!(ExplicitConsultMembers::new(Vec::<&str>::new()).is_err());
    assert!(ExplicitConsultMembers::new(["alpha", "alpha"]).is_err());
}

#[test]
fn member_manifest_binds_the_ordered_attachment_snapshot() {
    let manifest = ConsultMemberManifestV3::new(
        "alpha",
        "0123456789012345678901234567890123456789",
        "/repo",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        42,
    )
    .expect("manifest");
    assert!(manifest.matches_invocation(
        "0123456789012345678901234567890123456789",
        "/repo",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    ));
    assert!(!manifest.matches_invocation(
        "0123456789012345678901234567890123456789",
        "/repo",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    ));
}

fn assert_invalid(input: ConsultAnswerInputV3) {
    assert!(matches!(
        classify_consult_answer_v3(&input),
        ConsultAnswerValidityV3::Invalid(_)
    ));
}

#[test]
fn exit_zero_without_final_text_is_invalid() {
    assert_invalid(ConsultAnswerInputV3::from_terminal(
        terminal(None),
        "",
        false,
        false,
    ));
}

#[test]
fn tool_only_output_is_invalid() {
    assert_invalid(ConsultAnswerInputV3::from_terminal(
        terminal(Some("tool output only")),
        "tool output only",
        true,
        false,
    ));
}

#[test]
fn raw_transcript_is_invalid() {
    assert_invalid(ConsultAnswerInputV3::from_terminal(
        terminal(Some("raw transcript")),
        "raw transcript",
        false,
        true,
    ));
}

#[test]
fn missing_or_untrusted_terminal_is_invalid() {
    assert_invalid(ConsultAnswerInputV3::from_terminal(
        absent_terminal(),
        "FINAL: PASS",
        false,
        false,
    ));
}

#[test]
fn substantive_answer_with_trusted_terminal_is_valid() {
    let valid = ConsultAnswerInputV3::from_terminal(
        terminal(Some("FINAL: PASS")),
        "FINAL: PASS",
        false,
        false,
    );
    assert_eq!(
        classify_consult_answer_v3(&valid),
        ConsultAnswerValidityV3::Valid
    );
}

#[test]
fn mixed_failure_empty_tool_and_quoted_frames_are_saved_while_slow_work_is_still_running() {
    use std::{fs, path::Path, process::Command, time::{Duration, Instant}};
    use std::os::unix::fs::PermissionsExt;
    use orch_host::consult::{run_consultation, ConsultArgs};
    let root = orch_host::util::test_scratch_dir("b327-mixed-driver-outcomes");
    fs::create_dir_all(root.join(".orch")).unwrap();
    fs::create_dir_all(root.join("coordination")).unwrap();
    fs::write(root.join("question.md"), "Return a substantive final.\n").unwrap();
    fs::write(root.join("coordination/PROJECT-BINDING.yaml"), "data:\n  forbiddenArtifactPatterns: []\n").unwrap();
    fs::write(root.join(".gitignore"), ".orch/\n.cowork-temp/\ncoordination/consultations/\n").unwrap();
    let mut yaml = String::from("version: 1\nharnesses:\n");
    for (alias, driver, frame, exit, wait) in [
        ("broken", "opencode", r#"{"type":"error","error":{"type":"authentication_error","statusCode":401,"message":"credentials rejected"}}"#, 1, false),
        ("empty", "claude", r#"{"type":"result","subtype":"success","is_error":false,"result":"  "}"#, 0, false),
        ("tool", "claude", r#"{"type":"tool_result","content":"PASS from a tool is not a final"}"#, 0, false),
        ("quoted", "claude", r#"{"type":"result","subtype":"success","is_error":false,"result":"The example says upstream response stream failed and unauthorized; this is ordinary reviewed text."}"#, 0, false),
        ("slow", "claude", r#"{"type":"result","subtype":"success","is_error":false,"result":"Slow work completed normally."}"#, 0, true),
    ] {
        let pause = if wait { "touch slow-started\nwhile [ ! -f release-slow ]; do /bin/sleep 0.02; done\n" } else { "" };
        let executable = root.join(alias);
        fs::write(&executable, format!("#!/bin/sh\nset -eu\n{pause}printf '%s\\n' '{frame}'\nexit {exit}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        yaml.push_str(&format!("  {alias}:\n    driver: {driver}\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n", executable.display()));
    }
    fs::write(root.join(".orch/harnesses.yaml"), yaml).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git").arg("-C").arg(&root).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q"]); git(&["add", "."]);
    git(&["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]);
    let work = root.clone();
    let runner = std::thread::spawn(move || run_consultation(&work, &ConsultArgs {
        question: "question.md".into(), attachments: vec![],
        harnesses: ["broken","empty","tool","quoted","slow"].map(String::from).to_vec(),
        member_timeout_secs: Some(30), total_wall_secs: Some(30),
    }));
    let read = |path: &Path| fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let until = Instant::now() + Duration::from_secs(10);
    let mut early = None;
    while Instant::now() < until && early.is_none() {
        if root.join("slow-started").exists() {
            if let Ok(entries) = fs::read_dir(root.join("coordination/consultations")) {
                for entry in entries.flatten() {
                    let dir = entry.path();
                    let values = ["0-broken","1-empty","2-tool","3-quoted"].map(|name|
                        read(&dir.join(format!("fusion/{name}.manifest.json"))));
                    if values.iter().all(Option::is_some) {
                        early = Some((dir, values.map(Option::unwrap))); break;
                    }
                }
            }
        }
        if early.is_none() { std::thread::sleep(Duration::from_millis(20)); }
    }
    let summary_was_absent = early.as_ref().is_some_and(|(dir,_)| !dir.join("summary.md").exists());
    fs::write(root.join("release-slow"), b"release\n").unwrap();
    let outcome = runner.join().unwrap().unwrap();
    let (dir, values) = early.expect("all fast outcomes must be saved before slow release");
    assert!(summary_was_absent);
    assert_eq!(values[0]["failureClass"], "authentication");
    for value in &values[..3] { assert_eq!(value["status"], "failed"); }
    assert_eq!(values[3]["status"], "ok");
    assert!(values[3]["failureClass"].is_null());
    for (name, value) in ["0-broken","1-empty","2-tool","3-quoted"].iter().zip(values) {
        assert_eq!(read(&dir.join(format!("fusion/{name}.manifest.json"))).unwrap(), value);
    }
    assert_eq!(outcome.members.iter().filter(|member| member.status == orch_host::consult::MemberStatus::Ok).count(), 2);
    assert!(outcome.summary_path.is_file());
    fs::remove_dir_all(root).unwrap();
}
