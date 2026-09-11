//! r84 contract. Relocate byte-for-byte; do not change until this task is Recorded.
//! Evidence also requires real production entry, full signed gates and each named negative mutation.
//! M1 action-blind discovery; M2 ignore action limits; M3 chars instead of UTF-8 bytes;
//! M4 reverse timeout precedence; M5 reread snapshot; M6 accept zero/unknown limits.
use std::path::Path;
use orch_host::harness_config::{parse_harness_config_snapshot, HarnessAction, HarnessInvocationLimits};

const CONFIG: &str = r#"
version: 1
harnesses:
  writer:
    driver: agy
    executable: /bin/echo
    enabled: true
    cwdPolicy: project-root
  reviewer:
    driver: claude
    executable: /bin/echo
    enabled: true
    defaults: {model: opus, effort: max}
    consult:
      limits: {maxPromptBytes: 6, timeoutSeconds: 120}
    cwdPolicy: target-worktree
"#;

#[test]
fn discovery_is_action_specific_and_selected_limits_are_snapshotted() {
 let config=parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"),CONFIG).unwrap();
 let rows=config.discover_for_action(HarnessAction::Consult);
 assert_eq!(rows.len(),2);
 assert_eq!(rows.iter().find(|r|r.alias()=="writer").unwrap().availability().label(),"unsupported");
 let selected=config.resolve("reviewer",HarnessAction::Consult).unwrap();
 let limits: HarnessInvocationLimits=selected.limits().clone();
 assert_eq!(limits.max_prompt_bytes(),Some(6));
 assert_eq!(limits.timeout_seconds(),Some(120));
 assert!(limits.validate_prompt("中中").is_ok());
 assert!(limits.validate_prompt("中中x").is_err());
 assert_eq!(limits.deadline_seconds(Some(300),900,1800).unwrap(),300);
 assert_eq!(limits.deadline_seconds(None,900,1800).unwrap(),120);
 assert_eq!(limits.deadline_seconds(Some(5000),900,1800).unwrap(),1800);
 assert!(limits.deadline_seconds(Some(0),900,1800).is_err());
 assert_eq!(config.resolve("reviewer",HarnessAction::Review).unwrap().limits().max_prompt_bytes(),None);
}

#[test]
fn malformed_limits_are_not_silently_ignored() {
 for bad in ["maxPromptBytes: 0", "timeoutSeconds: 0", "maxPromptBytes: -1", "extra: 1"] {
  let text=CONFIG.replace("maxPromptBytes: 6, timeoutSeconds: 120",bad);
  assert!(parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"),&text).is_err(),"{bad}");
 }
}

#[test]
fn limits_are_rejected_in_defaults_and_production_prepare_enforces_the_cap() {
 let invalid=CONFIG.replace("defaults: {model: opus, effort: max}","defaults: {model: opus, effort: max, limits: {maxPromptBytes: 6}}");
 assert!(parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"),&invalid).is_err());
 let config=parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"),CONFIG).unwrap();
 let result=orch_host::channel::prepare_invocation(&config,orch_host::channel::InvocationRequest {
  alias:"reviewer".into(),action:orch_host::channel::InvocationAction::Consult,
  prompt:"中中x".into(),project_root:"/repo".into(),target_worktree:"/repo".into(),
  target_head:"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
  attachments:orch_host::channel::capture_attachment_manifest_v1(&[]).unwrap(),
 });
 assert!(result.is_err(),"production invocation preparation ignored the byte limit");
}
