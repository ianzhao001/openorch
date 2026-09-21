//! Shared Rust helper semantics: no clone validator or Python config interpreter.
use orch_host::openorch_helper::{configure_profile,attach_project,select_members,registered_projects};
use std::{fs,path::PathBuf,process::Command,os::unix::fs::PermissionsExt};
fn fixture()->(PathBuf,PathBuf,PathBuf) {
 let base=orch_host::util::test_scratch_dir("helper core");let project=base.join("project");fs::create_dir(&project).unwrap();
 assert!(Command::new("git").args(["init","-q"]).arg(&project).status().unwrap().success());
 assert!(Command::new("git").args(["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","commit","--allow-empty","-qm","base"]).current_dir(&project).status().unwrap().success());
 let exe=base.join("native");fs::write(&exe,"#!/bin/sh\nexit 0\n").unwrap();fs::set_permissions(&exe,fs::Permissions::from_mode(0o755)).unwrap();
 let profile=serde_json::json!({"version":1,"harnesses":{"alpha":{"driver":"claude","executable":exe,"enabled":true,"cwdPolicy":"project-root"},"beta":{"driver":"opencode","executable":exe,"enabled":true,"cwdPolicy":"project-root"}},"defaults":{"single":"alpha","fusion":["alpha","beta"]}});
 let input=base.join("profile-input.json");fs::write(&input,serde_json::to_vec(&profile).unwrap()).unwrap();(project,base.join("personal"),input)
}
#[test]
fn explicit_profile_replacement_keeps_a_verified_recoverable_backup() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();let original=fs::read(dir.join("profile.json")).unwrap();
 let mut updated:serde_json::Value=serde_json::from_slice(&original).unwrap();updated["defaults"]["single"]="beta".into();fs::write(&input,serde_json::to_vec(&updated).unwrap()).unwrap();
 let result=configure_profile(&root,&dir,&input,true).unwrap();let backup=PathBuf::from(result["backup"].as_str().expect("explicit recovery path"));
 assert_eq!(fs::read(&backup).unwrap(),original);assert_eq!(fs::metadata(&backup).unwrap().permissions().mode()&0o777,0o600);
 assert_eq!(select_members(&dir,"single",&[]).unwrap(),vec!["beta"]);
 configure_profile(&root,&dir,&backup,true).unwrap();assert_eq!(fs::read(dir.join("profile.json")).unwrap(),original);assert_eq!(fs::read(&backup).unwrap(),original);
}
#[test]
fn registry_rejects_replaced_project_until_explicit_reattach() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();attach_project(&root,&dir).unwrap();
 let retained=root.with_extension("retained");fs::rename(&root,&retained).unwrap();fs::create_dir(&root).unwrap();
 assert!(Command::new("git").args(["init","-q"]).arg(&root).status().unwrap().success());
 assert!(Command::new("git").args(["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","commit","--allow-empty","-qm","replacement"]).current_dir(&root).status().unwrap().success());
 assert!(registered_projects(&dir).is_err());attach_project(&root,&dir).unwrap();assert_eq!(registered_projects(&dir).unwrap(),vec![fs::canonicalize(&root).unwrap()]);
 assert!(retained.join(".orch/harnesses.yaml").is_file());
}
#[test]
fn concurrent_attach_serializes_configuration_and_registration() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();
 std::thread::scope(|scope| {
  let a=scope.spawn(||attach_project(&root,&dir));let b=scope.spawn(||attach_project(&root,&dir));let values=[a.join().unwrap().unwrap(),b.join().unwrap().unwrap()];
  assert_eq!(values.iter().filter(|v|v["created"]==true).count(),1);
 });
 assert_eq!(registered_projects(&dir).unwrap(),vec![fs::canonicalize(&root).unwrap()]);
 orch_host::harness_config::load_harness_config_snapshot(&root).unwrap().lint_without_tokens().unwrap();
}
#[test]
fn unsupported_default_and_symlink_input_preserve_profile_bytes() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();let original=fs::read(dir.join("profile.json")).unwrap();
 let mut invalid:serde_json::Value=serde_json::from_slice(&original).unwrap();invalid["harnesses"]["alpha"]["driver"]="agy".into();fs::write(&input,serde_json::to_vec(&invalid).unwrap()).unwrap();
 assert!(configure_profile(&root,&dir,&input,true).is_err());assert_eq!(fs::read(dir.join("profile.json")).unwrap(),original);
 let link=input.with_extension("link");std::os::unix::fs::symlink(&input,&link).unwrap();assert!(configure_profile(&root,&dir,&link,true).is_err());assert_eq!(fs::read(dir.join("profile.json")).unwrap(),original);
}
#[test]
fn symlink_project_state_is_not_followed_or_overwritten() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();let outside=dir.join("outside");fs::create_dir(&outside).unwrap();fs::write(outside.join("sentinel"),"keep").unwrap();
 std::os::unix::fs::symlink(&outside,root.join(".orch")).unwrap();assert!(attach_project(&root,&dir).is_err());assert!(!outside.join("harnesses.yaml").exists());assert_eq!(fs::read_to_string(outside.join("sentinel")).unwrap(),"keep");
}
#[test]
fn configuration_validation_never_invokes_the_native_client() {
 let (root,dir,input)=fixture();let native=root.parent().unwrap().join("native");fs::write(&native,"#!/usr/bin/python3\nfrom pathlib import Path\nPath(__file__).with_name('called').write_text('called')\n").unwrap();
 configure_profile(&root,&dir,&input,false).unwrap();attach_project(&root,&dir).unwrap();assert!(!native.with_file_name("called").exists());
}

#[test]
fn public_setup_contract_explains_recovery_and_project_authority() {
 let guide=include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");let section=guide.split("## Rust setup and packaged MCP registration").nth(1).unwrap();
 for term in ["same Rust configuration parser","atomic exchange","actual overwritten","projects.json","cwd is not authority","seven-resource inventory","six public CLI leaves"] {assert!(section.contains(term),"missing setup contract: {term}");}
}
