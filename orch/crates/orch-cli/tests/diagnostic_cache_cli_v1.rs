//! Public CLI reachability and the durable pre-spawn ordering contract.
#![cfg(feature = "selfhost")]
use std::{fs,path::{Path,PathBuf},process::Command};
use std::os::unix::fs::MetadataExt;

fn manifest(root:&Path)->Vec<(PathBuf,u64,u64,u64,std::time::SystemTime,Vec<u8>)> {
    let mut result=Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path=entry.unwrap().path();let m=fs::symlink_metadata(&path).unwrap();
        result.push((path.clone(),m.dev(),m.ino(),m.len(),m.modified().unwrap(),if m.is_file(){fs::read(&path).unwrap()}else{vec![]}));
        if m.is_dir() && !m.file_type().is_symlink() {result.extend(manifest(&path));}
    }
    result.sort_by(|a,b|a.0.cmp(&b.0));result
}

#[test]
fn status_is_reachable_without_a_round_and_writes_nothing() {
    let root=orch_host::util::test_scratch_dir("diagnostic-cli-status");
    assert!(Command::new("git").args(["init","-q"]).arg(&root).status().unwrap().success());
    let before=manifest(&root);
    let output=Command::new(env!("CARGO_BIN_EXE_orch"))
        .arg("--root").arg(&root).args(["sites","cache","status"]).current_dir(&root).output().unwrap();
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let rows:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();assert_eq!(rows,serde_json::json!([]));
    assert_eq!(manifest(&root),before);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn durable_registration_syncs_before_the_real_spawn_call() {
    // A filesystem test cannot simulate loss of the host's page cache. Pin the
    // actual fsync/rename order as well as the real child-side registration test.
    let source=fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../orch-host/src/buildcache.rs")).unwrap();
    let save=source.split("fn diagnostic_save(").nth(1).unwrap().split("fn diagnostic_git(").next().unwrap();
    let file_sync=save.find("file.sync_all()").expect("record bytes synced");
    let rename=save.find("fs::rename(").expect("atomic record publication");
    let directory_sync=save.find("File::open(dir)?.sync_all()").expect("published entry synced");
    assert!(file_sync<rename && rename<directory_sync);
    let run=source.split("pub fn run_managed_diagnostic_with_retention(").nth(1).unwrap().split("fn diagnostic_outside_inventory(").next().unwrap();
    let run: String = run.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(run.find("diagnostic_save(&dir,&record)?").unwrap()<run.find("command.spawn()").unwrap());
    assert!(run.find("File::open(&registry)?.sync_all()").unwrap()<run.find("command.spawn()").unwrap());
}
