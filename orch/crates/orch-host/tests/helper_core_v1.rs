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
fn configure_uses_core_schema_and_preserves_previous_profile_on_error() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();let saved=fs::read(dir.join("profile.json")).unwrap();
 let mut value:serde_json::Value=serde_json::from_slice(&saved).unwrap();value["harnesses"]["alpha"]["arbitraryCommand"]="touch sentinel".into();fs::write(&input,serde_json::to_vec(&value).unwrap()).unwrap();
 assert!(configure_profile(&root,&dir,&input,true).is_err());assert_eq!(fs::read(dir.join("profile.json")).unwrap(),saved);
 assert_eq!(fs::metadata(dir.join("profile.json")).unwrap().permissions().mode()&0o777,0o600);
}
#[test]
fn attach_preserves_existing_bytes_and_registers_only_explicit_project() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();attach_project(&root,&dir).unwrap();
 let target=root.join(".orch/harnesses.yaml");let mut custom=fs::read(&target).unwrap();custom.extend_from_slice(b"\n  ");fs::write(&target,&custom).unwrap();attach_project(&root,&dir).unwrap();
 assert_eq!(fs::read(&target).unwrap(),custom);assert_eq!(registered_projects(&dir).unwrap(),vec![fs::canonicalize(&root).unwrap()]);assert!(!root.join(".gitignore").exists());
}
#[test]
fn explicit_members_do_not_rewrite_personal_defaults_or_allow_duplicates() {
 let (root,dir,input)=fixture();configure_profile(&root,&dir,&input,false).unwrap();let saved=fs::read(dir.join("profile.json")).unwrap();
 assert_eq!(select_members(&dir,"single",&[]).unwrap(),vec!["alpha"]);assert_eq!(select_members(&dir,"fusion",&[]).unwrap(),vec!["alpha","beta"]);
 assert_eq!(select_members(&dir,"single",&["beta".into()]).unwrap(),vec!["beta"]);
 assert!(select_members(&dir,"fusion",&["alpha".into(),"alpha".into()]).is_err());assert_eq!(fs::read(dir.join("profile.json")).unwrap(),saved);
}
