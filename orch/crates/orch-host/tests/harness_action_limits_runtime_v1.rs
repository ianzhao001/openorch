use std::path::Path;
use orch_host::channel::{capture_attachment_manifest_v1, prepare_invocation, InvocationAction, InvocationRequest};
use orch_host::harness_config::{load_harness_config_snapshot, parse_harness_config_snapshot, HarnessAction};

fn config(limits: &str) -> String {
    format!("version: 1\nharnesses:\n  alpha:\n    driver: claude\n    executable: /bin/echo\n    enabled: true\n    defaults: {{model: opus, effort: max}}\n    consult:\n      limits: {limits}\n    cwdPolicy: project-root\n")
}

fn request(prompt: &str) -> InvocationRequest {
    InvocationRequest {
        alias: "alpha".into(), action: InvocationAction::Consult, prompt: prompt.into(),
        project_root: "/repo".into(), target_worktree: "/repo".into(),
        target_head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        attachments: capture_attachment_manifest_v1(&[]).unwrap(),
    }
}

#[test]
fn prepared_limits_and_digests_remain_on_the_captured_snapshot() {
    let root = orch_host::util::test_scratch_dir("b326-snapshot-limits");
    std::fs::create_dir_all(root.join(".orch")).unwrap();
    std::fs::write(root.join(".gitignore"), ".orch/harnesses.yaml\n").unwrap();
    assert!(std::process::Command::new("git").arg("-C").arg(&root).args(["init", "-q"]).status().unwrap().success());
    let path = root.join(".orch/harnesses.yaml");
    std::fs::write(&path, config("{maxPromptBytes: 6, timeoutSeconds: 120}")).unwrap();
    let old = load_harness_config_snapshot(&root).unwrap();
    std::fs::write(&path, config("{maxPromptBytes: 3, timeoutSeconds: 60}")).unwrap();
    let next = load_harness_config_snapshot(&root).unwrap();
    let from_root = |text: &str| {
        let mut value = request(text);
        value.project_root = root.clone();
        value.target_worktree = root.clone();
        value
    };
    let prepared = prepare_invocation(&old, from_root("中中")).unwrap();
    assert_eq!(prepared.limits().max_prompt_bytes(), Some(6));
    assert_eq!(prepared.limits().timeout_seconds(), Some(120));
    assert_eq!(prepared.config_digest(), old.sha256());
    assert_ne!(old.sha256(), next.sha256());
    assert!(prepare_invocation(&next, from_root("中中")).is_err());
    let old_small = prepare_invocation(&old, from_root("中")).unwrap();
    let new_small = prepare_invocation(&next, from_root("中")).unwrap();
    assert_ne!(old_small.request_digest(), new_small.request_digest());
    assert_eq!(old.resolve("alpha", HarnessAction::Review).unwrap().limits().max_prompt_bytes(), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn present_invalid_limits_fail_instead_of_becoming_absent() {
    for limits in ["null", "[]", "{maxPromptBytes: null}", "{timeoutSeconds: null}", "{timeoutSeconds: 1.5}", "{maxPromptBytes: true}", "{timeoutSeconds: 18446744073709551616}", "{unknown: 5}"] {
        assert!(parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), &config(limits)).is_err(), "{limits}");
    }
}
