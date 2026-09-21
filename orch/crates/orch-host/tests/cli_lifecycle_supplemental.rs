//! Supplemental shared lifecycle and evidence-layout boundary tests.
use orch_host::{
    channel::InvocationTuple,
    consult::ConsultArgs,
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
fn partial_cli_failure_preserves_verified_sibling_and_legacy_log() {
    let root=fixture();let engine=FusionEngine::new();let mut input=args(&root);input.harnesses.push("missing".into());
    let result=engine.run_cli(&root,&input).unwrap();
    assert_eq!(result.members.len(),2);assert_eq!(result.members[0].status,orch_host::consult::MemberStatus::Ok);
    assert_eq!(result.members[1].status,orch_host::consult::MemberStatus::Failed);
    let view=engine.read_status(&root,&result.id).unwrap();assert_eq!(view.phase,"failed");
    assert_eq!(engine.read_answer_page(&root,&result.id,&view.members[0].role_id,0,100,None).unwrap().text,"verified original answer");
    let log=fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
    let rows=log.lines().map(|l|serde_json::from_str::<serde_json::Value>(l).unwrap()).collect::<Vec<_>>();
    assert_eq!(rows.len(),1);assert_eq!(rows[0]["kind"],"ConsultationCompleted");assert_eq!(rows[0]["membersOk"],1);assert_eq!(rows[0]["membersFailed"],1);
}
#[test]
fn planted_alternate_snapshot_cannot_hide_modern_snapshot_tampering() {
    let root=fixture();let engine=FusionEngine::new();
    engine.start_consult(&root,ConsultRequest {request_id:"modern".into(),question:"Use supplied facts.".into(),members:vec![FusionRole{id:"one".into(),name:"One".into(),harness:"configured:alpha".into(),instructions:"Read only.".into(),fixed:InvocationTuple::default()}],attachments:vec![]}).unwrap();
    engine.drain_jobs();assert_eq!(engine.read_status(&root,"modern").unwrap().phase,"completed");
    let dir=root.join(".orch/fusion-runs/modern");let original=fs::read(dir.join("harness-snapshot.json")).unwrap();
    fs::write(dir.join("harness-snapshot.yaml"),original).unwrap();fs::write(dir.join("harness-snapshot.json"),"tampered").unwrap();
    assert!(engine.read_status(&root,"modern").is_err());
}
#[test]
fn native_routing_snapshot_tampering_invalidates_cli_completion() {
    let root=fixture();let engine=FusionEngine::new();let result=engine.run_cli(&root,&args(&root)).unwrap();
    fs::write(root.join(".orch/fusion-runs").join(&result.id).join("harness-native.json"),"tampered").unwrap();
    assert!(engine.read_status(&root,&result.id).is_err());
}
#[test]
fn secret_literal_is_rejected_before_shared_reservation_files() {
    let root=fixture();let engine=FusionEngine::new();
    let result=engine.start_consult(&root,ConsultRequest {request_id:"secret".into(),question:"OPENAI_API_KEY=sk-test-secret-material-do-not-store".into(),members:vec![FusionRole{id:"one".into(),name:"One".into(),harness:"configured:alpha".into(),instructions:"Read only.".into(),fixed:InvocationTuple::default()}],attachments:vec![]});
    assert!(result.is_err());assert!(!root.join(".orch/fusion-runs/secret").exists());assert!(!root.join("calls").exists());
}

#[test]
fn public_guide_explains_shared_lifecycle_and_failure_recovery() {
    let guide=include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    let section=guide.split("## Shared CLI consultation lifecycle").nth(1).expect("public shared lifecycle documentation");
    for contract in ["coordination/consultations/<id>/fusion/", "no mirrored answer", "captured", "partial-success", "HOLD", "never relabels", "cannot bypass"] {
        assert!(section.contains(contract), "missing public contract: {contract}");
    }
}
