//! r85 B334: valid-repository dry-run succeeds and preserves recursive file metadata and bytes.
use orch_host::reclaim::maintain_storage;
use std::{path::Path,fs,process::Command};
fn manifest(root:&Path)->Vec<(String,u64,std::time::SystemTime,Vec<u8>)>{
 let mut v=Vec::new();for e in fs::read_dir(root).unwrap(){let p=e.unwrap().path();let m=fs::symlink_metadata(&p).unwrap();v.push((p.display().to_string(),m.len(),m.modified().unwrap(),if m.is_file(){fs::read(&p).unwrap()}else{vec![]}));if m.is_dir(){v.extend(manifest(&p));}}v.sort_by(|a,b|a.0.cmp(&b.0));v
}
#[test]
fn maintenance_dry_run_succeeds_without_recursive_writes(){
 let root=orch_host::util::test_scratch_dir("b334-dry-run");
 assert!(Command::new("git").args(["init","-q"]).arg(&root).status().unwrap().success());
 fs::create_dir_all(root.join("coordination/runtime/evidence")).unwrap();
 fs::write(root.join("coordination/runtime/evidence/sentinel"),b"historical evidence").unwrap();
 let before=manifest(&root);maintain_storage(&root,true).expect("valid repository dry run must succeed");assert_eq!(manifest(&root),before);
}
