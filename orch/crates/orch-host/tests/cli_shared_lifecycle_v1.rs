//! CLI compatibility must be backed by the same durable engine as MCP and Web.
use orch_host::{
    channel::InvocationTuple,
    consult::{run_consultation, ConsultArgs},
    fusion_roles::FusionRole,
    fusion_run::{ConsultRequest, FusionEngine},
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

fn fixture() -> PathBuf {
    let root = orch_host::util::test_scratch_dir("CLI shared lifecycle");
    for args in [
        vec!["init", "-q"],
        vec!["-c", "user.name=Fixture", "-c", "user.email=f@example.invalid",
             "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgSign=false",
             "commit", "--allow-empty", "-qm", "base"],
    ] {
        assert!(Command::new("/usr/bin/git").args(args).current_dir(&root).status().unwrap().success());
    }
    fs::create_dir(root.join(".orch")).unwrap();
    fs::write(root.join(".gitignore"), ".orch/\n.cowork-temp/\ncoordination/consultations/\n").unwrap();
    fs::write(root.join("question.md"), "Assess the supplied facts.").unwrap();
    let exe = root.join("fake-claude");
    fs::write(&exe, r#"#!/usr/bin/python3
import json,time
from pathlib import Path
root=Path(__file__).parent
with (root/'calls').open('a') as f:f.write('prompt\n')
while (root/'wait').exists() and not (root/'release').exists():time.sleep(.02)
print(json.dumps({'type':'system','subtype':'init','model':'fixture-flash'}),flush=True)
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':'verified original answer'}),flush=True)
"#).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join(".orch/harnesses.yaml"), format!(
        "version: 1\nharnesses:\n  alpha:\n    driver: claude\n    executable: {}\n    enabled: true\n    defaults: {{model: fixture-flash, mode: plan}}\n    cwdPolicy: project-root\n", exe.display()
    )).unwrap();
    root
}
fn args(root: &std::path::Path) -> ConsultArgs {
    ConsultArgs { question: root.join("question.md"), harnesses: vec!["alpha".into()],
        attachments: vec![], member_timeout_secs: Some(3), total_wall_secs: Some(8) }
}
#[test]
fn synchronous_cli_uses_shared_state_and_preserves_legacy_artifacts() {
    let root = fixture(); let engine = FusionEngine::new();
    let outcome = engine.run_cli(&root, &args(&root)).unwrap();
    let view = engine.read_status(&root, &outcome.id).unwrap();
    assert_eq!(view.phase, "completed");
    assert_eq!(outcome.members[0].member, "alpha");
    assert_eq!(outcome.dir, root.join("coordination/consultations").join(&outcome.id));
    assert!(outcome.dir.join("fusion/0-alpha.manifest.json").is_file());
    assert!(outcome.summary_path.is_file());
    let page = engine.read_answer_page(&root, &outcome.id, &view.members[0].role_id, 0, 100, None).unwrap();
    assert_eq!(page.text, "verified original answer");
}
#[test]
fn public_legacy_entry_registers_the_same_durable_run() {
    let root = fixture(); let outcome = run_consultation(&root, &args(&root)).unwrap();
    let view = FusionEngine::new().read_status(&root, &outcome.id).unwrap();
    assert_eq!(view.phase, "completed");
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap().lines().count(), 1);
}
#[test]
fn visible_cli_answer_tampering_is_rejected_by_shared_reader() {
    let root = fixture(); let engine = FusionEngine::new();
    let outcome = engine.run_cli(&root, &args(&root)).unwrap();
    let view = engine.read_status(&root, &outcome.id).unwrap();
    fs::write(outcome.dir.join("fusion/0-alpha.md"), "forged answer").unwrap();
    assert!(engine.read_answer_page(&root, &outcome.id, &view.members[0].role_id, 0, 100, None).is_err());
}
#[test]
fn explicit_api_and_cli_share_one_project_owner() {
    let root = fixture(); fs::write(root.join("wait"), "wait").unwrap();
    let engine = FusionEngine::new();
    engine.start_consult(&root, ConsultRequest {
        request_id: "owner".into(), question: "Use supplied facts.".into(),
        members: vec![FusionRole { id: "one".into(), name: "One".into(),
            harness: "configured:alpha".into(), instructions: "Read only.".into(),
            fixed: InvocationTuple::default() }], attachments: vec![],
    }).unwrap();
    let rejected = engine.run_cli(&root, &args(&root));
    fs::write(root.join("release"), "release").unwrap(); engine.drain_jobs();
    assert!(rejected.is_err());
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap().lines().count(), 1);
}
