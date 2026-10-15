//! Additional recovery/ownership cases through the production retirement and close APIs.
#![cfg(feature="selfhost")]
use orch_host::storage_policy::{assess,preservation_digest,retire_manifest};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::{fs,path::{Path,PathBuf},process::Command,os::unix::fs::{MetadataExt,FileTypeExt,PermissionsExt,symlink}};
struct Fixture(PathBuf);
impl Drop for Fixture { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }
fn git(p:&Path,args:&[&str])->String {
    let o=Command::new("git").args(["-c","core.fsmonitor=false","-c","core.hooksPath=/dev/null",
        "-c","gc.auto=0","-c","maintenance.auto=false","-c","user.name=Fixture","-c","user.email=f@example.invalid","-C"])
        .arg(p).args(args).output().unwrap();
    assert!(o.status.success(),"{}",String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn fixture(label:&str)->Fixture {
    let r=orch_host::util::test_scratch_dir(label);
    git(&r,&["init","-q","-b","main"]);
    fs::write(r.join(".gitignore"),".cowork-temp/\n.worktrees/\ncoordination/runtime/\n").unwrap();
    fs::write(r.join("tracked.txt"),"original\n").unwrap();
    git(&r,&["add","."]);git(&r,&["commit","-qm","fixture"]);
    closed(&r,"r90");Fixture(r)
}
fn closed(r:&Path,round:&str) {
    let d=r.join(format!("coordination/rounds/{round}"));fs::create_dir_all(&d).unwrap();
    let e=orch_host::ledger::event("RoundClosed","runtime:orch",None,Some(round),json!({"forced":false,"unrecorded":[]}));
    fs::write(d.join("events.jsonl"),format!("{}\n",serde_json::to_string(&e).unwrap())).unwrap();
}
fn fifo(path:&Path,mode:u32) {
    assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    fs::set_permissions(path,fs::Permissions::from_mode(mode)).unwrap();
}
fn copy_tree(from:&Path,to:&Path,exclude:&[&str]) {
    fn inner(base:&Path,from:&Path,to:&Path,exclude:&[&str]) {
        if exclude.iter().any(|x|from.strip_prefix(base).unwrap().starts_with(x)) {return;}
        let m=fs::symlink_metadata(from).unwrap();
        if m.file_type().is_symlink() {symlink(fs::read_link(from).unwrap(),to).unwrap();}
        else if m.is_dir() {fs::create_dir_all(to).unwrap();for e in fs::read_dir(from).unwrap(){let e=e.unwrap();inner(base,&e.path(),&to.join(e.file_name()),exclude);}fs::set_permissions(to,m.permissions()).unwrap();}
        else if m.file_type().is_fifo() {fifo(to,m.mode() & 0o7777);}
        else {fs::copy(from,to).unwrap();fs::set_permissions(to,m.permissions()).unwrap();}
    }
    inner(from,from,to,exclude)
}
fn entry(r:&Path,path:&str,backup:&str)->Value {
    let p=r.join(path);let m=fs::metadata(&p).unwrap();
    json!({"path":path,"round":"r90","device":m.dev(),"inode":m.ino(),"head":null,"kind":"directory",
        "sourceDigest":preservation_digest(&p,&[]).unwrap(),"backupPath":backup,"excludedCaches":[]})
}
fn manifest(r:&Path,e:Value)->PathBuf {
    let p=r.join("retire.json");fs::write(&p,serde_json::to_vec(&json!({"schemaVersion":1,"projectRoot":r,
        "authorization":"fixture explicit exact preserved retirement","expiresAt":4102444800u64,"entries":[e]})).unwrap()).unwrap();p
}
fn plain(r:&Path)->(PathBuf,Value) {
    let p=r.join(".cowork-temp/old");fs::create_dir_all(&p).unwrap();fs::write(p.join("data"),b"unique").unwrap();
    let backup="coordination/runtime/site-archives/plain/source";copy_tree(&p,&r.join(backup),&[]);
    (p,entry(r,".cowork-temp/old",backup))
}
fn receipt(r:&Path,m:&Path,e:&Value,phase:&str)->(PathBuf,PathBuf) {
    let id=hex::encode(Sha256::digest(fs::read(m).unwrap()));
    let p=r.join(format!("coordination/runtime/storage-maintenance/retirements/{id}.json"));
    let q=r.join(e["path"].as_str().unwrap()).parent().unwrap().join(format!(".retiring-{id}"));
    let mut complete=e.clone();complete["gitPreservation"]=Value::Null;complete["preservationRoot"]=Value::Null;complete["archivePath"]=Value::Null;
    fs::create_dir_all(p.parent().unwrap()).unwrap();fs::write(&p,serde_json::to_vec(&json!({"schemaVersion":1,"manifestSha256":id,"entry":complete,
        "device":e["device"],"inode":e["inode"],"phase":phase,"quarantine":q})).unwrap()).unwrap();(p,q)
}
#[test]
fn completed_replay_rejects_aba_and_changed_backup() {
    let f=fixture("b373-aba");let(p,e)=plain(&f.0);let m=manifest(&f.0,e.clone());
    retire_manifest(&f.0,&m,false).unwrap();fs::create_dir(&p).unwrap();
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("ABA"));fs::remove_dir(&p).unwrap();
    fs::write(f.0.join(e["backupPath"].as_str().unwrap()).join("data"),b"changed").unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());
}
#[test]
fn rename_before_receipt_is_recoverable_and_partial_delete_holds() {
    let f=fixture("b373-rename-recovery");let(p,e)=plain(&f.0);let m=manifest(&f.0,e.clone());
    let(_,q)=receipt(&f.0,&m,&e,"prepared");fs::rename(&p,&q).unwrap();
    assert_eq!(retire_manifest(&f.0,&m,false).unwrap()["items"][0]["disposition"],"removed");
    let f=fixture("b373-partial-recovery");let(p,e)=plain(&f.0);let m=manifest(&f.0,e.clone());
    let(_,q)=receipt(&f.0,&m,&e,"renamed");fs::rename(&p,&q).unwrap();fs::remove_file(q.join("data")).unwrap();
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("HOLD"));
    assert!(q.exists());assert!(f.0.join(e["backupPath"].as_str().unwrap()).join("data").exists());
}
#[test]
fn absent_needs_prior_receipt_and_never_claims_new_deletion() {
    let f=fixture("b373-absent");let(p,e)=plain(&f.0);let m=manifest(&f.0,e.clone());fs::remove_dir_all(&p).unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());receipt(&f.0,&m,&e,"renamed");
    let v=retire_manifest(&f.0,&m,false).unwrap();assert_eq!(v["items"][0]["disposition"],"reconciled-absent");
    assert_eq!(v["items"][0]["newlyRemovedBytes"],0);
}
#[test]
fn no_mutable_hardlinked_backup_and_no_symlinked_ancestor() {
    let f=fixture("b373-hardlink-backup");let(p,e)=plain(&f.0);let b=f.0.join(e["backupPath"].as_str().unwrap()).join("data");
    fs::remove_file(&b).unwrap();fs::hard_link(p.join("data"),&b).unwrap();let m=manifest(&f.0,e);
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("inodes"));assert!(p.exists());
    let parent=f.0.join(".cowork-temp");let other=f.0.join("saved");fs::rename(&parent,&other).unwrap();symlink(&other,&parent).unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());assert!(other.join("old/data").exists());
}
#[test]
fn snapshot_owner_and_recent_round_cannot_be_relabelled() {
    let f=fixture("b373-owner");let(p,mut e)=plain(&f.0);let named=f.0.join(".cowork-temp/collect-source-owned");fs::rename(p,&named).unwrap();
    e["path"]=json!(".cowork-temp/collect-source-owned");let n=fs::metadata(&named).unwrap();
    let record=json!({"schemaVersion":1,"projectRoot":f.0,"path":named,"device":n.dev(),"inode":n.ino(),
        "owner":{"round":"r95","taskId":"T","attemptId":"T-A0001","purpose":"collect"},"phase":"retained"});
    fs::write(named.with_extension("receipt.json"),serde_json::to_vec(&record).unwrap()).unwrap();
    let m=manifest(&f.0,e.clone());assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("contradicts"));
    closed(&f.0,"r95");e["round"]=json!("r95");let m=manifest(&f.0,e);
    fs::create_dir_all(f.0.join("coordination/runtime")).unwrap();fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r96\n").unwrap();
    fs::write(f.0.join("coordination/STORAGE-POLICY.yaml"),"schemaVersion: 1\nmaxAllocatedBytes: 50000000000\nkeepRecentRounds: 3\n").unwrap();
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("retained round"));
}
#[test]
fn independent_git_preserves_dirty_staged_untracked_and_symlink_data() {
    let f=fixture("b373-dirty-git");let p=f.0.join(".cowork-temp/old");fs::create_dir_all(p.parent().unwrap()).unwrap();
    git(&f.0,&["clone","-q","--no-local",f.0.to_str().unwrap(),p.to_str().unwrap()]);
    fs::write(p.join("tracked.txt"),"staged\n").unwrap();git(&p,&["add","tracked.txt"]);fs::write(p.join("tracked.txt"),"unstaged\n").unwrap();
    fs::write(p.join("unique"),"unique\n").unwrap();symlink("unique",p.join("link")).unwrap();
    let b="coordination/runtime/site-archives/git/source";copy_tree(&p,&f.0.join(b),&[]);
    let mut e=entry(&f.0,".cowork-temp/old",b);e["head"]=json!(git(&p,&["rev-parse","HEAD"]));
    let before=git(&p,&["status","--porcelain=v1","--untracked-files=all"]);let m=manifest(&f.0,e);
    retire_manifest(&f.0,&m,false).unwrap();assert_eq!(before,git(&f.0.join(b),&["status","--porcelain=v1","--untracked-files=all"]));
    assert_eq!(git(&f.0.join(b),&["show",":tracked.txt"]),"staged");
    assert_eq!(fs::read_to_string(f.0.join(b).join("tracked.txt")).unwrap(),"unstaged\n");
    assert_eq!(fs::read_link(f.0.join(b).join("link")).unwrap(),PathBuf::from("unique"));
}
#[test]
fn retained_round_can_reclaim_only_the_preserved_git_cargo_target() {
    let f=fixture("b373-cache-only");closed(&f.0,"r95");
    let source=f.0.join(".cowork-temp/collect-source-recent");fs::create_dir_all(source.parent().unwrap()).unwrap();
    git(&f.0,&["clone","-q","--no-local",f.0.to_str().unwrap(),source.to_str().unwrap()]);
    fs::write(source.join(".git/info/exclude"),"orch/target/\n").unwrap();fs::create_dir_all(source.join("orch/target")).unwrap();
    fs::write(source.join("orch/target/cache"),vec![1;16384]).unwrap();
    let b="coordination/runtime/site-archives/recent/source";copy_tree(&source,&f.0.join(b),&["orch/target"]);
    let src=fs::metadata(&source).unwrap();let target=source.join("orch/target");let t=fs::metadata(&target).unwrap();
    let e=json!({"path":".cowork-temp/collect-source-recent/orch/target","round":"r95","device":t.dev(),"inode":t.ino(),
        "head":git(&source,&["rev-parse","HEAD"]),"kind":"directory","sourceDigest":preservation_digest(&source,&["orch/target".into()]).unwrap(),
        "backupPath":b,"excludedCaches":["orch/target"],"preservationRoot":{"path":".cowork-temp/collect-source-recent","device":src.dev(),"inode":src.ino()}});
    fs::write(source.with_extension("receipt.json"),serde_json::to_vec(&json!({"schemaVersion":1,"projectRoot":f.0,"path":source,
        "device":src.dev(),"inode":src.ino(),"owner":{"round":"r95","taskId":"T","attemptId":"T-A0001","purpose":"collect"},"phase":"retained"})).unwrap()).unwrap();
    fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r96\n").unwrap();
    fs::write(f.0.join("coordination/STORAGE-POLICY.yaml"),"schemaVersion: 1\nmaxAllocatedBytes: 50000000000\nkeepRecentRounds: 3\n").unwrap();
    let m=manifest(&f.0,e);retire_manifest(&f.0,&m,false).unwrap();assert!(source.join("tracked.txt").exists());assert!(!target.exists());
    assert_eq!(assess(&f.0,"r96").unwrap().unwrap()["expiredPaths"],json!([]));
}
#[test]
fn linked_worktree_requires_full_git_backup_and_restores_index_and_unique_data() {
    let f=fixture("b373-linked-restore");let p=f.0.join(".worktrees/old");
    git(&f.0,&["worktree","add","-q","-b","old",p.to_str().unwrap(),"HEAD"]);
    fs::write(p.join("tracked.txt"),"staged\n").unwrap();git(&p,&["add","tracked.txt"]);
    fs::write(p.join("tracked.txt"),"unstaged\n").unwrap();fs::write(p.join("unique"),"keep me").unwrap();
    let base="coordination/runtime/site-archives/linked";
    let source=format!("{base}/source");let common=format!("{base}/common");let private=format!("{base}/private");let restored=format!("{base}/restored");
    let actual_private=PathBuf::from(git(&p,&["rev-parse","--path-format=absolute","--git-dir"]));
    copy_tree(&p,&f.0.join(&source),&[]);copy_tree(&f.0.join(".git"),&f.0.join(&common),&[]);
    copy_tree(&actual_private,&f.0.join(&private),&[]);copy_tree(&p,&f.0.join(&restored),&[".git"]);
    copy_tree(&f.0.join(&common),&f.0.join(&restored).join(".git"),&["worktrees"]);
    for file in ["HEAD","index"] {fs::copy(actual_private.join(file),f.0.join(&restored).join(".git").join(file)).unwrap();}
    let mut e=entry(&f.0,".worktrees/old",&source);e["head"]=json!(git(&p,&["rev-parse","HEAD"]));e["kind"]=json!("linked-worktree");
    let missing=manifest(&f.0,e.clone());assert!(retire_manifest(&f.0,&missing,false).unwrap_err().to_string().contains("full common/private"));
    e["gitPreservation"]=json!({"commonBackup":common,"commonDigest":preservation_digest(&f.0.join(".git"),&[]).unwrap(),
        "privateBackup":private,"privateDigest":preservation_digest(&actual_private,&[]).unwrap(),"restoredWorktree":restored});
    let m=manifest(&f.0,e);let before=git(&p,&["status","--porcelain=v1","--untracked-files=all"]);
    assert_eq!(retire_manifest(&f.0,&m,true).unwrap()["items"][0]["disposition"],"eligible");
    retire_manifest(&f.0,&m,false).unwrap();assert!(!p.exists());assert!(!actual_private.exists());
    assert_eq!(git(&f.0.join(&restored),&["status","--porcelain=v1","--untracked-files=all"]),before);
    assert_eq!(git(&f.0.join(&restored),&["show",":tracked.txt"]),"staged");
    assert_eq!(fs::read_to_string(f.0.join(&restored).join("unique")).unwrap(),"keep me");
    assert_eq!(retire_manifest(&f.0,&m,false).unwrap()["items"][0]["disposition"],"already-removed");
}

#[test]
fn nested_linked_git_cannot_bypass_private_and_common_preservation() {
    let f=fixture("b373-nested-git");let p=f.0.join(".cowork-temp/container");fs::create_dir_all(&p).unwrap();
    git(&f.0,&["worktree","add","-q","--detach",p.join("nested").to_str().unwrap(),"HEAD"]);
    let b="coordination/runtime/site-archives/nested/source";copy_tree(&p,&f.0.join(b),&[]);
    let e=entry(&f.0,".cowork-temp/container",b);let m=manifest(&f.0,e);
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("nested Git"));
    assert!(p.join("nested/.git").exists());
}

#[test]
fn excluded_cache_never_discards_an_independent_git_repository() {
    let f=fixture("b373-cache-git-safety");
    let source=f.0.join(".cowork-temp/collect-source-cache-safety");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    git(&f.0,&["clone","-q","--no-local",f.0.to_str().unwrap(),source.to_str().unwrap()]);
    fs::write(source.join(".git/info/exclude"),"orch/target/\n").unwrap();
    let target=source.join("orch/target");fs::create_dir_all(&target).unwrap();
    let backup="coordination/runtime/site-archives/cache-git/source";
    copy_tree(&source,&f.0.join(backup),&["orch/target"]);
    let unique=target.join("experiment");fs::create_dir(&unique).unwrap();git(&unique,&["init","-q","-b","main"]);
    fs::write(unique.join("unique.txt"),"not disposable\n").unwrap();git(&unique,&["add","."]);git(&unique,&["commit","-qm","unique"]);
    let sm=fs::metadata(&source).unwrap();let tm=fs::metadata(&target).unwrap();
    let e=json!({"path":".cowork-temp/collect-source-cache-safety/orch/target","round":"r90",
        "device":tm.dev(),"inode":tm.ino(),"head":git(&source,&["rev-parse","HEAD"]),"kind":"directory",
        "sourceDigest":preservation_digest(&source,&["orch/target".into()]).unwrap(),"backupPath":backup,"excludedCaches":["orch/target"],
        "preservationRoot":{"path":".cowork-temp/collect-source-cache-safety","device":sm.dev(),"inode":sm.ino()}});
    let m=manifest(&f.0,e);
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("excluded cache"));
    assert_eq!(fs::read_to_string(unique.join("unique.txt")).unwrap(),"not disposable\n");
    assert!(unique.join(".git").exists());
}

#[test]
fn debug_only_cleanup_preserves_git_fixtures_and_intact_archive_restores_their_original_paths() {
    let f=fixture("b373-preserve-fixture-topology");
    let source=f.0.join(".cowork-temp/collect-source-older");fs::create_dir_all(source.parent().unwrap()).unwrap();
    git(&f.0,&["clone","-q","--no-local",f.0.to_str().unwrap(),source.to_str().unwrap()]);
    fs::write(source.join(".git/info/exclude"),"orch/target/\n").unwrap();
    let debug=source.join("orch/target/debug");fs::create_dir_all(&debug).unwrap();fs::write(debug.join("compiled"),vec![1;8192]).unwrap();
    let fixture=source.join("orch/target/test-tmp/fixture");fs::create_dir_all(&fixture).unwrap();git(&fixture,&["init","-q","-b","main"]);
    fs::write(fixture.join("unique"),"preserved fixture\n").unwrap();git(&fixture,&["add","."]);git(&fixture,&["commit","-qm","fixture"]);
    let child=source.join("orch/target/test-tmp/linked");git(&fixture,&["worktree","add","-q","--detach",child.to_str().unwrap(),"HEAD"]);
    let broken=source.join("orch/target/test-tmp/broken");fs::create_dir(&broken).unwrap();fs::write(broken.join(".git"),"historical invalid marker\n").unwrap();
    let pipe=source.join("orch/target/test-tmp/historical-fifo");fifo(&pipe,0o640);
    let pipe_identity=fs::symlink_metadata(&pipe).unwrap();
    let backup="coordination/runtime/site-archives/topology/source";copy_tree(&source,&f.0.join(backup),&["orch/target/debug"]);
    let dm=fs::metadata(&debug).unwrap();let sm=fs::metadata(&source).unwrap();
    let mut e=json!({"path":".cowork-temp/collect-source-older/orch/target/debug","round":"r90","device":dm.dev(),"inode":dm.ino(),
        "head":git(&source,&["rev-parse","HEAD"]),"kind":"directory","sourceDigest":preservation_digest(&source,&["orch/target/debug".into()]).unwrap(),
        "backupPath":backup,"excludedCaches":["orch/target/debug"],"preservationRoot":{"path":".cowork-temp/collect-source-older","device":sm.dev(),"inode":sm.ino()}});
    let m=manifest(&f.0,e.clone());retire_manifest(&f.0,&m,false).unwrap();assert!(!debug.exists());
    assert!(fixture.join("unique").exists());let nested_head=git(&child,&["rev-parse","HEAD"]);
    e["path"]=json!(".cowork-temp/collect-source-older");e["device"]=json!(sm.dev());e["inode"]=json!(sm.ino());
    e["preservationRoot"]=Value::Null;e["excludedCaches"]=json!([]);e["sourceDigest"]=json!(preservation_digest(&source,&[]).unwrap());
    e["archivePath"]=json!("coordination/runtime/site-archives/topology/original-scene");
    let m=manifest(&f.0,e.clone());let before=preservation_digest(&f.0,&[]).unwrap();
    assert_eq!(retire_manifest(&f.0,&m,true).unwrap()["items"][0]["disposition"],"eligible-archive");
    assert_eq!(preservation_digest(&f.0,&[]).unwrap(),before);
    let v=retire_manifest(&f.0,&m,false).unwrap();assert_eq!(v["items"][0]["disposition"],"archived");assert_eq!(v["items"][0]["newlyRemovedBytes"],0);
    let archive=f.0.join(e["archivePath"].as_str().unwrap());assert!(!source.exists());assert!(archive.join("orch/target/test-tmp/fixture/unique").exists());
    let archived_pipe=archive.join("orch/target/test-tmp/historical-fifo");
    let saved_pipe=fs::symlink_metadata(&archived_pipe).unwrap();
    assert!(saved_pipe.file_type().is_fifo());assert_eq!(saved_pipe.mode() & 0o7777,0o640);
    assert_eq!((saved_pipe.dev(),saved_pipe.ino()),(pipe_identity.dev(),pipe_identity.ino()));
    assert_eq!(retire_manifest(&f.0,&m,false).unwrap()["items"][0]["disposition"],"already-archived");
    // Actual original-path recovery reactivates all internal absolute Git pointers.
    fs::rename(&archive,&source).unwrap();assert_eq!(git(&child,&["rev-parse","HEAD"]),nested_head);
    assert_eq!(fs::read_to_string(broken.join(".git")).unwrap(),"historical invalid marker\n");
    assert!(fs::symlink_metadata(&pipe).unwrap().file_type().is_fifo());
    assert_eq!(fs::symlink_metadata(&pipe).unwrap().mode() & 0o7777,0o640);
    assert_eq!(preservation_digest(&source,&[]).unwrap(),e["sourceDigest"].as_str().unwrap());
    assert!(retire_manifest(&f.0,&m,false).is_err(),"restoration is never silently re-archived by an old receipt");
}
#[test]
fn intact_archive_never_clobbers_and_recovers_rename_before_completion_receipt() {
    let f=fixture("b373-archive-recovery");let(source,mut e)=plain(&f.0);
    let relative="coordination/runtime/site-archives/plain/original-scene";
    let destination=f.0.join(relative);e["archivePath"]=json!(relative);let m=manifest(&f.0,e.clone());
    fs::create_dir(&destination).unwrap();fs::write(destination.join("sentinel"),"keep").unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());assert!(source.exists());
    assert_eq!(fs::read_to_string(destination.join("sentinel")).unwrap(),"keep");
    fs::remove_file(destination.join("sentinel")).unwrap();fs::remove_dir(&destination).unwrap();
    let id=hex::encode(Sha256::digest(fs::read(&m).unwrap()));
    let journal=f.0.join(format!("coordination/runtime/storage-maintenance/retirements/{id}.json"));
    e["gitPreservation"]=Value::Null;e["preservationRoot"]=Value::Null;
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(&journal,serde_json::to_vec(&json!({"schemaVersion":1,"manifestSha256":id,"entry":e,
        "device":e["device"],"inode":e["inode"],"phase":"archive-prepared","archivePath":destination})).unwrap()).unwrap();
    fs::rename(&source,&destination).unwrap();
    let result=retire_manifest(&f.0,&m,false).unwrap();
    assert_eq!(result["items"][0]["disposition"],"reconciled-archive");
    assert_eq!(result["items"][0]["newlyRemovedBytes"],0);
    assert_eq!(fs::read(destination.join("data")).unwrap(),b"unique");
}

#[test]
fn fifo_backup_must_preserve_type_mode_and_presence_and_other_sockets_stay_unsupported() {
    use std::os::unix::net::UnixListener;
    let f=fixture("b373-fifo-preservation");
    let p=f.0.join(".cowork-temp/old");fs::create_dir_all(&p).unwrap();fifo(&p.join("pipe"),0o640);
    let backup="coordination/runtime/site-archives/fifo/source";copy_tree(&p,&f.0.join(backup),&[]);
    let mut e=entry(&f.0,".cowork-temp/old",backup);
    e["archivePath"]=json!("coordination/runtime/site-archives/fifo/original-scene");
    let m=manifest(&f.0,e);let saved=f.0.join(backup).join("pipe");
    fs::set_permissions(&saved,fs::Permissions::from_mode(0o600)).unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());assert!(p.exists());
    fs::remove_file(&saved).unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());assert!(p.exists());
    fs::write(&saved,[]).unwrap();fs::set_permissions(&saved,fs::Permissions::from_mode(0o640)).unwrap();
    assert!(retire_manifest(&f.0,&m,false).is_err());assert!(p.exists());
    fs::remove_file(&saved).unwrap();fifo(&saved,0o640);
    assert_eq!(preservation_digest(&p,&[]).unwrap(),preservation_digest(&f.0.join(backup),&[]).unwrap());
    // A pipe with no writer is hashed without opening its stream. Normal sockets remain HOLD.
    let socket=f.0.join("ordinary.sock");
    // sockaddr_un limits the supplied name, so use a short relative path without changing cwd.
    let cwd=std::env::current_dir().unwrap();
    let common=cwd.ancestors().find(|p|socket.starts_with(p)).unwrap();
    let mut relative=PathBuf::new();
    for _ in cwd.strip_prefix(common).unwrap().components() {relative.push("..");}
    relative.push(socket.strip_prefix(common).unwrap());
    let _listener=UnixListener::bind(&relative).unwrap();
    assert!(preservation_digest(&f.0,&[]).unwrap_err().to_string().contains("unsupported preserved file type"));
}

fn custody_fixture() -> (Fixture, PathBuf, Value, PathBuf) {
    let f=fixture("b373-custody");closed(&f.0,"r95");
    let source=f.0.join(".cowork-temp/diagnostic-source");fs::create_dir_all(source.parent().unwrap()).unwrap();
    git(&f.0,&["clone","-q","--no-local",f.0.to_str().unwrap(),source.to_str().unwrap()]);
    let head=git(&source,&["rev-parse","HEAD"]);let sm=fs::metadata(&source).unwrap();
    let relative="coordination/runtime/diagnostic-cache/01M3528DC7132VG781ZCS0JEZN/record.json";
    let record=f.0.join(relative);fs::create_dir_all(record.parent().unwrap()).unwrap();
    let v=json!({"schemaVersion":1,"id":"01M3528DC7132VG781ZCS0JEZN","root":f.0,"cwd":source,"head":head,
        "executable":"/usr/bin/true","executableSha256":"a".repeat(64),"args":[],"purpose":"r95 fixture diagnostic",
        "device":1,"inode":2,"phase":"removed","pid":null,"exitCode":0,"stdout":null,"stderr":null,"removalInferredFromAbsence":false});
    fs::write(&record,serde_json::to_vec(&v).unwrap()).unwrap();
    let policy=json!({"schemaVersion":1,"maxAllocatedBytes":50000000000u64,"keepRecentRounds":3,
        "roundCustody":[{"path":".cowork-temp/diagnostic-source","round":"r95","device":sm.dev(),"inode":sm.ino(),"head":head,
          "evidencePath":relative,"evidenceSha256":hex::encode(Sha256::digest(fs::read(&record).unwrap()))}]});
    (f,source,policy,record)
}
fn write_custody_policy(root:&Path,policy:&Value) {
    fs::write(root.join("coordination/STORAGE-POLICY.yaml"),serde_json::to_vec(policy).unwrap()).unwrap();
}
#[test]
fn round_custody_classifies_only_bound_source_and_expires_without_deletion_authority() {
    let(f,source,policy,_)=custody_fixture();write_custody_policy(&f.0,&policy);
    let v=assess(&f.0,"r97").unwrap().unwrap();assert_eq!(v["policySatisfied"],true);
    assert!(v["projectAllocatedBytes"].as_u64().unwrap()>0);
    let v=assess(&f.0,"r98").unwrap().unwrap();assert_eq!(v["expiredPaths"],json!([".cowork-temp/diagnostic-source"]));
    assert_eq!(v["policySatisfied"],false);assert!(source.exists());
    let unknown=f.0.join(".cowork-temp/still-unknown");fs::create_dir(&unknown).unwrap();
    let v=assess(&f.0,"r97").unwrap().unwrap();assert_eq!(v["unclassifiedPaths"],json!([".cowork-temp/still-unknown"]));
    assert_eq!(v["policySatisfied"],false);fs::remove_dir(&unknown).unwrap();
    let backup="coordination/runtime/site-archives/custody/source";copy_tree(&source,&f.0.join(backup),&[]);
    let mut e=entry(&f.0,".cowork-temp/diagnostic-source",backup);e["head"]=policy["roundCustody"][0]["head"].clone();
    fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r97\n").unwrap();
    let m=manifest(&f.0,e.clone());assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("custody"));
    e["round"]=json!("r95");let m=manifest(&f.0,e);
    assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("retained round"));assert!(source.exists());
}
#[test]
fn round_custody_rejects_source_evidence_and_claim_drift() {
    let(f,source,policy,record)=custody_fixture();write_custody_policy(&f.0,&policy);
    let original=fs::read(&record).unwrap();let mut drifted:Value=serde_json::from_slice(&original).unwrap();
    drifted["args"]=json!(["changed-but-valid-diagnostic"]);fs::write(&record,serde_json::to_vec(&drifted).unwrap()).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("digest"));fs::write(&record,&original).unwrap();
    let mut changed=policy.clone();changed["roundCustody"][0]["inode"]=json!(0);
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=policy.clone();changed["roundCustody"][0]["head"]=json!("0".repeat(40));
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=policy.clone();changed["roundCustody"][0]["round"]=json!("r98");
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=policy.clone();changed["roundCustody"].as_array_mut().unwrap().push(policy["roundCustody"][0].clone());
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    write_custody_policy(&f.0,&policy);assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    let e=orch_host::ledger::event("WorkspaceLeased","runtime:orch",Some("T"),Some("r90"),json!({"siteId":"T-implement-local-g01",
      "generation":1,"attemptId":"T-A0001","role":"implement","agent":"local","reviewedHead":policy["roundCustody"][0]["head"],
      "paths":{"worktree":".cowork-temp/diagnostic-source","target":".cowork-temp/diagnostic-source/orch/target"}}));
    use std::io::Write;
    writeln!(fs::OpenOptions::new().append(true).open(f.0.join("coordination/rounds/r90/events.jsonl")).unwrap(),"{}",serde_json::to_string(&e).unwrap()).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("conflicts with existing claim"));assert!(source.exists());
}

#[test]
fn round_custody_cannot_adopt_incomplete_or_reappeared_diagnostics_or_an_open_round() {
    let(f,_,policy,record)=custody_fixture();write_custody_policy(&f.0,&policy);
    let original=fs::read(&record).unwrap();let mut v:Value=serde_json::from_slice(&original).unwrap();
    v.as_object_mut().unwrap().remove("executable");let bytes=serde_json::to_vec(&v).unwrap();fs::write(&record,&bytes).unwrap();
    let mut altered=policy.clone();altered["roundCustody"][0]["evidenceSha256"]=json!(hex::encode(Sha256::digest(&bytes)));
    write_custody_policy(&f.0,&altered);assert!(assess(&f.0,"r97").is_err());
    fs::write(&record,&original).unwrap();write_custody_policy(&f.0,&policy);
    let target=record.parent().unwrap().join("target");fs::create_dir(&target).unwrap();
    assert!(assess(&f.0,"r97").is_err());fs::remove_dir(&target).unwrap();
    let ledger=f.0.join("coordination/rounds/r95/events.jsonl");
    let opened=orch_host::ledger::event("RoundOpened","runtime:orch",None,Some("r95"),json!({"purpose":"valid open fixture","contractSchemaVersion":3}));
    fs::write(&ledger,format!("{}\n",serde_json::to_string(&opened).unwrap())).unwrap();
    assert_eq!(orch_host::round::contract_schema_at_root(&f.0,"r95").unwrap(),Some(3));
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("open"));
}

fn custody_recovery(archive_mode:bool) {
    let(f,source,policy,_)=custody_fixture();write_custody_policy(&f.0,&policy);
    let backup="coordination/runtime/site-archives/custody-recovery/source";copy_tree(&source,&f.0.join(backup),&[]);
    let mut e=entry(&f.0,".cowork-temp/diagnostic-source",backup);e["round"]=json!("r95");
    e["head"]=policy["roundCustody"][0]["head"].clone();
    fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r98\n").unwrap();
    if archive_mode {
        let destination=f.0.join("coordination/runtime/site-archives/custody-recovery/original-scene");
        e["archivePath"]=json!(destination.strip_prefix(&f.0).unwrap().to_str().unwrap());
        let m=manifest(&f.0,e.clone());let id=hex::encode(Sha256::digest(fs::read(&m).unwrap()));
        let journal=f.0.join(format!("coordination/runtime/storage-maintenance/retirements/{id}.json"));
        fs::rename(&source,&destination).unwrap();
        assert!(retire_manifest(&f.0,&m,false).is_err(),"custody never substitutes for a prior receipt");
        assert!(assess(&f.0,"r98").is_err(),"ordinary assessment still requires live custody identity");
        fs::rename(&destination,&source).unwrap();
        e["gitPreservation"]=Value::Null;e["preservationRoot"]=Value::Null;
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(&journal,serde_json::to_vec(&json!({"schemaVersion":1,"manifestSha256":id,"entry":e,
            "device":e["device"],"inode":e["inode"],"phase":"archive-prepared","archivePath":destination})).unwrap()).unwrap();
        fs::rename(&source,&destination).unwrap();
        let original=fs::read(destination.join("tracked.txt")).unwrap();
        fs::write(destination.join("tracked.txt"),"changed after rename").unwrap();
        assert!(retire_manifest(&f.0,&m,false).is_err());
        fs::write(destination.join("tracked.txt"),original).unwrap();
        let v=retire_manifest(&f.0,&m,false).unwrap();assert_eq!(v["items"][0]["disposition"],"reconciled-archive");
        assert_eq!(v["items"][0]["newlyRemovedBytes"],0);
        assert_eq!(retire_manifest(&f.0,&m,false).unwrap()["items"][0]["disposition"],"already-archived");
        assert_eq!(preservation_digest(&destination,&[]).unwrap(),e["sourceDigest"].as_str().unwrap());
    } else {
        let m=manifest(&f.0,e.clone());let(_,quarantine)=receipt(&f.0,&m,&e,"renamed");
        fs::rename(&source,&quarantine).unwrap();
        let original=fs::read(quarantine.join("tracked.txt")).unwrap();
        fs::write(quarantine.join("tracked.txt"),"changed after rename").unwrap();
        assert!(retire_manifest(&f.0,&m,false).is_err());
        fs::write(quarantine.join("tracked.txt"),original).unwrap();
        assert_eq!(retire_manifest(&f.0,&m,false).unwrap()["items"][0]["disposition"],"removed");
        assert!(!quarantine.exists());assert!(!source.exists());
        assert_eq!(retire_manifest(&f.0,&m,false).unwrap()["items"][0]["disposition"],"already-removed");
        assert_eq!(preservation_digest(&f.0.join(backup),&[]).unwrap(),e["sourceDigest"].as_str().unwrap());
    }
}
#[test]
fn round_custody_recovers_archive_rename_before_completion_receipt() {custody_recovery(true);}
#[test]
fn round_custody_recovers_quarantine_rename_before_completion_receipt() {custody_recovery(false);}

fn legacy_fixture() -> (Fixture, PathBuf, Value) {
    let f=fixture("b373-legacy-collect");
    let source=f.0.join(".cowork-temp/collect-source-2147483647-0");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    git(&f.0,&["clone","-q","--no-local",f.0.to_str().unwrap(),source.to_str().unwrap()]);
    let head=git(&source,&["rev-parse","HEAD"]);let tree=git(&source,&["rev-parse","HEAD^{tree}"]);
    let sm=fs::metadata(&source).unwrap();let round=f.0.join("coordination/rounds/r97");fs::create_dir_all(&round).unwrap();
    let opened=orch_host::ledger::event("RoundOpened","runtime:orch",None,Some("r97"),json!({"purpose":"legacy fixture","contractSchemaVersion":3}));
    let collected=orch_host::ledger::event("ReportCollectCompleted","runtime:orch",Some("B373"),Some("r97"),json!({"attemptId":"B373-A0004","branchSha":head}));
    fs::write(round.join("events.jsonl"),format!("{}\n{}\n",serde_json::to_string(&opened).unwrap(),serde_json::to_string(&collected).unwrap())).unwrap();
    let log="coordination/runtime/logs/r97/seal.log";fs::create_dir_all(f.0.join(log).parent().unwrap()).unwrap();
    fs::write(f.0.join(log),format!("collect snapshot retained for incomplete/failed collection at {}\n",source.display())).unwrap();
    let log_sha=hex::encode(Sha256::digest(fs::read(f.0.join(log)).unwrap()));
    let receipt="coordination/runtime/logs/r97/seal-exit.json";
    let v=json!({"task":"B373","attempt":"B373-A0004","candidateHead":head,"pid":2147483647u64,"exitCode":5,
        "finishedAt":"2026-09-28T03:07:47Z","log":f.0.join(log),"logSha256":log_sha});
    fs::write(f.0.join(receipt),serde_json::to_vec(&v).unwrap()).unwrap();
    let policy=json!({"schemaVersion":1,"maxAllocatedBytes":50000000000u64,"keepRecentRounds":3,
        "legacyCollectSourceCustody":[{"path":source.strip_prefix(&f.0).unwrap(),"round":"r97","task":"B373","attempt":"B373-A0004",
            "device":sm.dev(),"inode":sm.ino(),"head":head,"tree":tree,"exitReceiptPath":receipt,
            "exitReceiptSha256":hex::encode(Sha256::digest(fs::read(f.0.join(receipt)).unwrap())),"logPath":log,"logSha256":log_sha}]});
    (f,source,policy)
}
#[test]
fn legacy_custody_retains_open_round_source_and_expires_without_deletion_authority() {
    let(f,source,policy)=legacy_fixture();write_custody_policy(&f.0,&policy);
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    let v=assess(&f.0,"r100").unwrap().unwrap();assert_eq!(v["policySatisfied"],false);
    assert_eq!(v["expiredPaths"],json!([policy["legacyCollectSourceCustody"][0]["path"]]));
    assert!(v["projectAllocatedBytes"].as_u64().unwrap()>0);assert!(source.exists());
    let backup="coordination/runtime/site-archives/legacy/source";copy_tree(&source,&f.0.join(backup),&[]);
    let mut e=entry(&f.0,policy["legacyCollectSourceCustody"][0]["path"].as_str().unwrap(),backup);
    e["round"]=json!("r97");e["head"]=policy["legacyCollectSourceCustody"][0]["head"].clone();
    fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r100\n").unwrap();
    let m=manifest(&f.0,e);assert!(retire_manifest(&f.0,&m,false).unwrap_err().to_string().contains("neither closed nor a Recorded"));assert!(source.exists());
    let unknown=f.0.join(".cowork-temp/unknown-legacy");fs::create_dir(&unknown).unwrap();
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],false);
}
#[test]
fn legacy_custody_rejects_identity_and_collection_drift() {
    let(f,source,policy)=legacy_fixture();write_custody_policy(&f.0,&policy);
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    for (key,value) in [("inode",json!(0)),("device",json!(0)),("head",json!("0".repeat(40))),
        ("tree",json!("0".repeat(40))),("round",json!("r96")),("task",json!("Other")),("attempt",json!("B373-A0003")),
        ("path",json!(".cowork-temp/../collect-source-2147483647-0"))] {
        let mut changed=policy.clone();changed["legacyCollectSourceCustody"][0][key]=value;
        write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err(),"accepted {key} drift");
    }
    write_custody_policy(&f.0,&policy);fs::write(source.join("tracked.txt"),"dirty").unwrap();
    assert!(assess(&f.0,"r97").is_err());fs::write(source.join("tracked.txt"),"original\n").unwrap();
    let ledger=f.0.join("coordination/rounds/r97/events.jsonl");let original=fs::read_to_string(&ledger).unwrap();
    fs::write(&ledger,original.lines().take(1).collect::<Vec<_>>().join("\n")+"\n").unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("matching collected"));
    fs::write(&ledger,original).unwrap();assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
}
#[test]
fn legacy_custody_rejects_proof_drift_and_mismatched_terminal() {
    let(f,_,policy)=legacy_fixture();let c=&policy["legacyCollectSourceCustody"][0];
    let receipt=f.0.join(c["exitReceiptPath"].as_str().unwrap());let log=f.0.join(c["logPath"].as_str().unwrap());
    let original=fs::read(&receipt).unwrap();let log_original=fs::read(&log).unwrap();
    write_custody_policy(&f.0,&policy);let mut changed_bytes=original.clone();changed_bytes.push(b' ');fs::write(&receipt,&changed_bytes).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("digest"));fs::write(&receipt,&original).unwrap();
    let mut changed_log=log_original.clone();changed_log.push(b'\n');fs::write(&log,&changed_log).unwrap();assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("digest"));fs::write(&log,&log_original).unwrap();
    for (key,value) in [("task",json!("Other")),("attempt",json!("B373-A0003")),("candidateHead",json!("0".repeat(40))),
        ("exitCode",json!(0)),("finishedAt",Value::Null),("log",json!("wrong")),("logSha256",json!("bad")),("pid",json!(1))] {
        let mut v:Value=serde_json::from_slice(&original).unwrap();v[key]=value;let bytes=serde_json::to_vec(&v).unwrap();fs::write(&receipt,&bytes).unwrap();
        let mut changed=policy.clone();changed["legacyCollectSourceCustody"][0]["exitReceiptSha256"]=json!(hex::encode(Sha256::digest(&bytes)));
        write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err(),"accepted terminal {key}");
    }
    fs::write(&receipt,&original).unwrap();let bad=b"retained some other source\n";fs::write(&log,bad).unwrap();
    let sha=hex::encode(Sha256::digest(bad));let mut v:Value=serde_json::from_slice(&original).unwrap();v["logSha256"]=json!(sha);
    let bytes=serde_json::to_vec(&v).unwrap();fs::write(&receipt,&bytes).unwrap();let mut changed=policy.clone();
    changed["legacyCollectSourceCustody"][0]["exitReceiptSha256"]=json!(hex::encode(Sha256::digest(&bytes)));
    changed["legacyCollectSourceCustody"][0]["logSha256"]=json!(sha);write_custody_policy(&f.0,&changed);
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("terminal/log"));
}
#[test]
fn legacy_custody_rejects_duplicates_claims_and_active_use() {
    let(f,source,policy)=legacy_fixture();let mut changed=policy.clone();
    changed["legacyCollectSourceCustody"].as_array_mut().unwrap().push(policy["legacyCollectSourceCustody"][0].clone());
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());write_custody_policy(&f.0,&policy);
    let held=fs::File::open(source.join("tracked.txt")).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("quiet"));drop(held);
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    let e=orch_host::ledger::event("WorkspaceLeased","runtime:orch",Some("T"),Some("r90"),json!({"siteId":"T-implement-local-g01",
        "generation":1,"attemptId":"T-A0001","role":"implement","agent":"local","reviewedHead":policy["legacyCollectSourceCustody"][0]["head"],
        "paths":{"worktree":policy["legacyCollectSourceCustody"][0]["path"],"target":format!("{}/orch/target",policy["legacyCollectSourceCustody"][0]["path"].as_str().unwrap())}}));
    use std::io::Write;writeln!(fs::OpenOptions::new().append(true).open(f.0.join("coordination/rounds/r90/events.jsonl")).unwrap(),"{}",serde_json::to_string(&e).unwrap()).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("existing claim"));
}
#[test]
fn legacy_custody_rejects_producer_receipt_and_cross_round_proof() {
    let(f,source,policy)=legacy_fixture();
    let original_receipt=f.0.join(policy["legacyCollectSourceCustody"][0]["exitReceiptPath"].as_str().unwrap());
    let receipt_bytes=fs::read(&original_receipt).unwrap();
    for key in ["exitReceiptPath","logPath"] {
        let mut changed=policy.clone();let rel="coordination/runtime/logs/r96/other.json";
        fs::create_dir_all(f.0.join(rel).parent().unwrap()).unwrap();
        let original=f.0.join(policy["legacyCollectSourceCustody"][0][key].as_str().unwrap());
        fs::copy(original,f.0.join(rel)).unwrap();changed["legacyCollectSourceCustody"][0][key]=json!(rel);
        if key=="logPath" {
            let mut v:Value=serde_json::from_slice(&receipt_bytes).unwrap();v["log"]=json!(f.0.join(rel));
            let bytes=serde_json::to_vec(&v).unwrap();fs::write(&original_receipt,&bytes).unwrap();
            changed["legacyCollectSourceCustody"][0]["exitReceiptSha256"]=json!(hex::encode(Sha256::digest(&bytes)));
        }
        write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("outside owner round"));
        fs::write(&original_receipt,&receipt_bytes).unwrap();
    }
    write_custody_policy(&f.0,&policy);let m=fs::metadata(&source).unwrap();
    let receipt=json!({"schemaVersion":1,"projectRoot":f.0,"path":source,"device":m.dev(),"inode":m.ino(),"owner":{"round":"r97"}});
    fs::write(source.with_extension("receipt.json"),serde_json::to_vec(&receipt).unwrap()).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("producer receipt"));
}
#[test]
fn legacy_custody_rejects_live_producer_even_with_bound_exit_receipt() {
    let(f,source,mut policy)=legacy_fixture();let pid=std::process::id();
    let renamed=source.parent().unwrap().join(format!("collect-source-{pid}-0"));fs::rename(&source,&renamed).unwrap();
    let c=&mut policy["legacyCollectSourceCustody"][0];c["path"]=json!(renamed.strip_prefix(&f.0).unwrap());
    let log=f.0.join(c["logPath"].as_str().unwrap());
    fs::write(&log,format!("collect snapshot retained for incomplete/failed collection at {}\n",renamed.display())).unwrap();
    let sha=hex::encode(Sha256::digest(fs::read(&log).unwrap()));c["logSha256"]=json!(sha);
    let receipt=f.0.join(c["exitReceiptPath"].as_str().unwrap());let mut v:Value=serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    v["pid"]=json!(pid);v["logSha256"]=json!(sha);let bytes=serde_json::to_vec(&v).unwrap();fs::write(&receipt,&bytes).unwrap();
    c["exitReceiptSha256"]=json!(hex::encode(Sha256::digest(&bytes)));write_custody_policy(&f.0,&policy);
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("producer/runner may still be present"));
}

fn failed_collection_fixture() -> (Fixture, PathBuf, Value, Vec<Value>) {
    let(f,source,old)=legacy_fixture();let c=&old["legacyCollectSourceCustody"][0];
    let mut identity=serde_json::Map::new();
    for k in ["path","round","task","attempt","device","inode","head","tree"] {identity.insert(k.into(),c[k].clone());}
    let payload=json!({"actionId":"a".repeat(64),"owner":"failed-owner","leaseGeneration":"failed-generation",
        "attemptId":"B373-A0004","attemptNo":4,"agent":"local","baseSha":c["head"],"goPath":"coordination/rounds/r97/dispatch/local/GO-B373-A0004.md",
        "evidencePath":f.0.join(".worktrees/B373/coordination/rounds/r97/reports/B373-REPORT.md"),"evidenceSha256":"b".repeat(64),"evidenceLen":128,"controlEpoch":"epoch"});
    let opened=orch_host::ledger::event("RoundOpened","runtime:orch",None,Some("r97"),json!({"purpose":"failed collection fixture","contractSchemaVersion":3}));
    let executing=orch_host::ledger::event("ReportCollectExecuting","runtime:orch",Some("B373"),Some("r97"),payload.clone());
    let gate=orch_host::ledger::event("GateExecuted","runtime:orch",Some("B373"),Some("r97"),json!({"phase":"trial","exitCode":101}));
    let released=orch_host::ledger::event("ReportCollectReleased","runtime:orch",Some("B373"),Some("r97"),payload.clone());
    let mut success=payload;success["owner"]=json!("later-owner");success["leaseGeneration"]=json!("later-generation");success["branchSha"]=c["head"].clone();
    let completed=orch_host::ledger::event("ReportCollectCompleted","runtime:orch",Some("B373"),Some("r97"),success);
    let policy=json!({"schemaVersion":1,"maxAllocatedBytes":50000000000u64,"keepRecentRounds":3,"legacyFailedCollectionCustody":[{
        "source":identity,"executingEventId":executing.event_id,"releasedEventId":released.event_id,"completedEventId":completed.event_id,
        "logPath":c["logPath"],"logBytes":fs::metadata(f.0.join(c["logPath"].as_str().unwrap())).unwrap().len(),"logSha256":c["logSha256"]}]});
    let events=vec![opened,executing,gate,released,completed].iter().map(|e|serde_json::to_value(e).unwrap()).collect::<Vec<_>>();
    write_failed_collection_events(&f.0,&events);(f,source,policy,events)
}
fn write_failed_collection_events(root:&Path,events:&[Value]) {
    fs::write(root.join("coordination/rounds/r97/events.jsonl"),events.iter().map(|e|serde_json::to_string(e).unwrap()+"\n").collect::<String>()).unwrap();
}
#[test]
fn failed_collection_custody_classifies_original_failure_without_granting_retirement() {
    let(f,source,policy,_)=failed_collection_fixture();write_custody_policy(&f.0,&policy);
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    let v=assess(&f.0,"r100").unwrap().unwrap();assert_eq!(v["expiredPaths"],json!([policy["legacyFailedCollectionCustody"][0]["source"]["path"]]));
    assert_eq!(v["policySatisfied"],false);assert!(source.exists());
    let backup="coordination/runtime/site-archives/failed-collection/source";copy_tree(&source,&f.0.join(backup),&[]);
    let c=&policy["legacyFailedCollectionCustody"][0]["source"];let mut e=entry(&f.0,c["path"].as_str().unwrap(),backup);e["round"]=json!("r97");e["head"]=c["head"].clone();
    fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r100\n").unwrap();
    assert!(retire_manifest(&f.0,&manifest(&f.0,e),false).unwrap_err().to_string().contains("neither closed nor a Recorded"));
    assert!(source.exists());assert!(!source.with_extension("receipt.json").exists());
}
#[test]
fn failed_collection_custody_rejects_crossed_or_uncertain_execution_lineage() {
    let(f,_,policy,events)=failed_collection_fixture();write_custody_policy(&f.0,&policy);
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    for (key,value) in [("owner",json!("other")),("leaseGeneration",json!("other")),("attemptId",json!("B373-A0003")),
        ("attemptNo",json!(3)),("agent",json!("other")),("baseSha",json!("0".repeat(40))),("goPath",json!("other")),
        ("evidenceSha256",json!("0".repeat(64))),("evidenceLen",json!(1)),("controlEpoch",json!("other")),("outcomeUnknown",json!(true))] {
        let mut changed=events.clone();changed[3]["payload"][key]=value;write_failed_collection_events(&f.0,&changed);
        assert!(assess(&f.0,"r97").is_err(),"accepted released {key}");
    }
    let mut changed=events.clone();changed[1]["type"]=json!("ReportCollectClaimed");write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=events.clone();changed[4]["payload"]["evidenceSha256"]=json!("0".repeat(64));write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=events.clone();changed[4]["payload"]["branchSha"]=json!("0".repeat(40));write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=events.clone();changed.swap(1,3);write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    write_failed_collection_events(&f.0,&events);assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
}
#[test]
fn failed_collection_custody_rejects_contradictory_success_and_missing_failed_gate() {
    let(f,_,policy,events)=failed_collection_fixture();write_custody_policy(&f.0,&policy);
    let mut changed=events.clone();changed[4]["payload"]["leaseGeneration"]=events[1]["payload"]["leaseGeneration"].clone();
    write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("contradictory"));
    changed=events.clone();changed[2]["payload"]["exitCode"]=json!(0);write_failed_collection_events(&f.0,&changed);
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("failed gate"));
    changed=events.clone();changed.push(events[3].clone());write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=events.clone();let mut duplicate=events[1].clone();duplicate["eventId"]=json!("01M4DYA96A418J25PDZ64AW3V5");changed.insert(2,duplicate);
    write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
}
#[test]
fn failed_collection_custody_rejects_log_source_and_use_drift() {
    let(f,source,policy,_)=failed_collection_fixture();write_custody_policy(&f.0,&policy);
    assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
    for key in ["device","inode"] {
        let mut changed=policy.clone();changed["legacyFailedCollectionCustody"][0]["source"][key]=json!(0);write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    }
    let mut changed=policy.clone();changed["legacyFailedCollectionCustody"][0]["logBytes"]=json!(1);write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    changed=policy.clone();changed["legacyFailedCollectionCustody"].as_array_mut().unwrap().push(policy["legacyFailedCollectionCustody"][0].clone());
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    write_custody_policy(&f.0,&policy);let held=fs::File::open(source.join("tracked.txt")).unwrap();
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("quiet"));drop(held);
    let path=f.0.join(policy["legacyFailedCollectionCustody"][0]["logPath"].as_str().unwrap());let bytes=fs::read(&path).unwrap();
    let mut changed_bytes=bytes.clone();changed_bytes.push(b'\n');fs::write(&path,&changed_bytes).unwrap();changed=policy.clone();
    changed["legacyFailedCollectionCustody"][0]["logBytes"]=json!(changed_bytes.len());write_custody_policy(&f.0,&changed);
    assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("digest"));
    let bad=b"collect snapshot retained for incomplete/failed collection at /wrong/source\n";fs::write(&path,bad).unwrap();
    changed["legacyFailedCollectionCustody"][0]["logBytes"]=json!(bad.len());changed["legacyFailedCollectionCustody"][0]["logSha256"]=json!(hex::encode(Sha256::digest(bad)));
    write_custody_policy(&f.0,&changed);assert!(assess(&f.0,"r97").unwrap_err().to_string().contains("retained source"));
}

#[test]
fn failed_collection_custody_requires_distinct_completed_owner_and_generation() {
    let(f,_,policy,events)=failed_collection_fixture();write_custody_policy(&f.0,&policy);
    for key in ["owner","leaseGeneration"] {
        for value in [Value::Null,json!(""),json!("   "),events[1]["payload"][key].clone()] {
            let mut changed=events.clone();changed[4]["payload"][key]=value;
            write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err(),"accepted invalid completed {key}");
        }
        let mut changed=events.clone();changed[4]["payload"].as_object_mut().unwrap().remove(key);
        write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err(),"accepted absent completed {key}");
    }
    let mut changed=events.clone();changed[3]["actor"]=json!("verifier:root");
    write_failed_collection_events(&f.0,&changed);assert!(assess(&f.0,"r97").is_err());
    write_failed_collection_events(&f.0,&events);assert_eq!(assess(&f.0,"r97").unwrap().unwrap()["policySatisfied"],true);
}

mod manual_lifecycle_support;

fn debug_entry(root:&Path,source:&Path,round:&str,head:&str,proof:Value,label:&str)->Value {
    let debug=source.join("orch/target/debug");fs::create_dir_all(&debug).unwrap();fs::write(debug.join("rebuildable"),"cache only").unwrap();
    fs::create_dir_all(source.join("orch/target/test-tmp")).unwrap();fs::write(source.join("orch/target/test-tmp/unique"),"retain me").unwrap();
    let backup=format!("coordination/runtime/site-archives/{label}/source");copy_tree(source,&root.join(&backup),&["orch/target/debug"]);
    let dm=fs::metadata(&debug).unwrap();let sm=fs::metadata(source).unwrap();
    json!({"path":debug.strip_prefix(root).unwrap(),"round":round,"device":dm.dev(),"inode":dm.ino(),"head":head,"kind":"directory",
        "sourceDigest":preservation_digest(source,&["orch/target/debug".into()]).unwrap(),"backupPath":backup,"excludedCaches":["orch/target/debug"],
        "preservationRoot":{"path":source.strip_prefix(root).unwrap(),"device":sm.dev(),"inode":sm.ino()},"legacyDebugRetirement":proof})
}
fn debug_manifest(root:&Path,e:&Value)->PathBuf {
    let p=root.join("coordination/runtime/debug-retire.json");fs::write(&p,serde_json::to_vec(&json!({"schemaVersion":1,"projectRoot":root,"authorization":"fixture explicit exact debug-only preservation","expiresAt":4102444800u64,"entries":[e]})).unwrap()).unwrap();p
}
fn recorded_debug_fixture(root:&Path)->(PathBuf,Value) {
    let events=orch_core::read_ledger(&root.join("coordination/rounds/r83/events.jsonl")).unwrap().events;
    let auth=orch_host::verify::validate_root_record_authorization(root,"r83","B901",&events).unwrap();assert!(auth.already_recorded);
    orch_host::close::validate_seal_postcondition(&events,"r83","B901",&auth.attempt_id).unwrap();
    let source=root.join(".cowork-temp/collect-source-2147483647-0");fs::create_dir_all(source.parent().unwrap()).unwrap();
    git(root,&["clone","-q","--no-local",root.to_str().unwrap(),source.to_str().unwrap()]);git(&source,&["switch","--detach",&auth.head_sha]);
    let sm=fs::metadata(&source).unwrap();let log="coordination/runtime/logs/r83/legacy-seal.log";let exit="coordination/runtime/logs/r83/legacy-seal-exit.json";
    fs::create_dir_all(root.join(log).parent().unwrap()).unwrap();fs::write(root.join(log),format!("collect snapshot retained for incomplete/failed collection at {}\n",source.display())).unwrap();
    let log_sha=hex::encode(Sha256::digest(fs::read(root.join(log)).unwrap()));
    fs::write(root.join(exit),serde_json::to_vec(&json!({"task":"B901","attempt":auth.attempt_id,"candidateHead":auth.head_sha,"pid":2147483647u64,"exitCode":5,"finishedAt":"fixture historical failure","log":root.join(log),"logSha256":log_sha})).unwrap()).unwrap();
    let c=json!({"path":source.strip_prefix(root).unwrap(),"round":"r83","task":"B901","attempt":auth.attempt_id,"device":sm.dev(),"inode":sm.ino(),"head":auth.head_sha,
        "tree":git(&source,&["rev-parse","HEAD^{tree}"]),"exitReceiptPath":exit,"exitReceiptSha256":hex::encode(Sha256::digest(fs::read(root.join(exit)).unwrap())),"logPath":log,"logSha256":log_sha});
    write_custody_policy(root,&json!({"schemaVersion":1,"maxAllocatedBytes":50000000000u64,"keepRecentRounds":3,"legacyCollectSourceCustody":[c]}));
    let view=assess(root,"r83").unwrap().unwrap();let rows=view["legacyCustody"].as_array().expect("validated custody descriptors required");
    let bound=rows.iter().find(|v|v["source"]["path"]==c["path"]).unwrap();
    let recorded=events.iter().find(|e|e.kind=="TaskRecorded" && e.task_id.as_deref()==Some("B901")).unwrap();
    let merge=events.iter().find(|e|e.kind=="MergeExecuted" && e.task_id.as_deref()==Some("B901")).unwrap();
    let proof=json!({"taskId":"B901","custodyKind":bound["custodyKind"],"custodySha256":bound["custodySha256"],"recordedEventId":recorded.event_id,"mergeExecutedEventId":merge.event_id,"recordedAttemptId":auth.attempt_id,"recordedHead":auth.head_sha});
    let e=debug_entry(root,&source,"r83",&auth.head_sha,proof,"legacy-debug");(source,e)
}
#[test]
#[ignore = "testExclusive:legacy_debug_retirement_requires_exact_recorded_proof"]
fn legacy_debug_retirement_requires_exact_recorded_proof() {
    manual_lifecycle_support::exercise(false,|root,_| {
        let(source,e)=recorded_debug_fixture(root);let m=debug_manifest(root,&e);
        assert_eq!(retire_manifest(root,&m,true).unwrap()["items"][0]["disposition"],"eligible");
        let mut absent=e.clone();absent.as_object_mut().unwrap().remove("legacyDebugRetirement");
        assert!(retire_manifest(root,&debug_manifest(root,&absent),false).is_err());
        for key in ["taskId","custodyKind","custodySha256","recordedEventId","mergeExecutedEventId","recordedAttemptId","recordedHead"] {
            let mut bad=e.clone();bad["legacyDebugRetirement"][key]=json!("invalid");assert!(retire_manifest(root,&debug_manifest(root,&bad),false).is_err(),"accepted {key}");
        }
        for suffix in ["","/orch/target","/orch/target/test-tmp"] {
            let mut bad=e.clone();bad["path"]=json!(format!("{}{}",source.strip_prefix(root).unwrap().display(),suffix));assert!(retire_manifest(root,&debug_manifest(root,&bad),false).is_err());
        }
        let mut bad=e.clone();bad["excludedCaches"]=json!(["orch/target"]);assert!(retire_manifest(root,&debug_manifest(root,&bad),false).is_err());
        // Widen all mutually consistent fields together: the old generic target path would allow
        // this without the new debug-only shape guard, including removal of the unique fixture.
        bad=e.clone();let target=source.join("orch/target");let tm=fs::metadata(&target).unwrap();
        bad["path"]=json!(target.strip_prefix(root).unwrap());bad["device"]=json!(tm.dev());bad["inode"]=json!(tm.ino());
        bad["excludedCaches"]=json!(["orch/target"]);bad["sourceDigest"]=json!(preservation_digest(&source,&["orch/target".into()]).unwrap());
        assert!(retire_manifest(root,&debug_manifest(root,&bad),false).is_err());

        bad=e.clone();bad["legacyDebugRetirement"]=json!(true);assert!(retire_manifest(root,&debug_manifest(root,&bad),false).is_err());
        let held=fs::File::open(source.join("src/lib.rs")).unwrap();assert!(retire_manifest(root,&debug_manifest(root,&e),false).is_err());drop(held);
        let producer=source.with_extension("receipt.json");
        fs::write(&producer,serde_json::to_vec(&json!({"schemaVersion":1,"projectRoot":root,"path":source,"owner":{"round":"r83","taskId":"B901"}})).unwrap()).unwrap();
        assert!(retire_manifest(root,&debug_manifest(root,&e),false).is_err());fs::remove_file(producer).unwrap();

        let marker=source.join("orch/target/debug/.git");fs::write(&marker,"unique Git marker").unwrap();assert!(retire_manifest(root,&debug_manifest(root,&e),false).is_err());fs::remove_file(marker).unwrap();
        let backup=root.join(e["backupPath"].as_str().unwrap());let kept=backup.join("src/lib.rs");let bytes=fs::read(&kept).unwrap();fs::remove_file(&kept).unwrap();fs::hard_link(source.join("src/lib.rs"),&kept).unwrap();
        assert!(retire_manifest(root,&debug_manifest(root,&e),false).is_err());fs::remove_file(&kept).unwrap();fs::write(&kept,bytes).unwrap();
        let m=debug_manifest(root,&e);assert_eq!(retire_manifest(root,&m,false).unwrap()["items"][0]["disposition"],"removed");
        assert!(source.exists());assert!(!source.join("orch/target/debug").exists());assert_eq!(fs::read_to_string(source.join("orch/target/test-tmp/unique")).unwrap(),"retain me");
        assert_eq!(retire_manifest(root,&m,false).unwrap()["items"][0]["disposition"],"already-removed");
        let unique=source.join("orch/target/test-tmp/unique");fs::write(&unique,"changed").unwrap();assert!(retire_manifest(root,&m,false).is_err());fs::write(unique,"retain me").unwrap();
        fs::create_dir(source.join("orch/target/debug")).unwrap();fs::write(source.join("orch/target/debug/new"),"new cache").unwrap();
        assert!(retire_manifest(root,&m,false).unwrap_err().to_string().contains("ABA"));
    });
}
#[test]
#[ignore = "testExclusive:legacy_debug_retirement_recovery_rechecks_preserved_source"]
fn legacy_debug_retirement_recovery_rechecks_preserved_source() {
    manual_lifecycle_support::exercise(false,|root,_| {
        let(source,e)=recorded_debug_fixture(root);let m=debug_manifest(root,&e);let id=hex::encode(Sha256::digest(fs::read(&m).unwrap()));
        let cache=source.join("orch/target/debug");let q=source.parent().unwrap().join(format!(".retiring-{id}"));
        fs::rename(&cache,&q).unwrap();assert!(retire_manifest(root,&m,false).is_err());fs::rename(&q,&cache).unwrap();
        let mut stored=e.clone();stored["gitPreservation"]=Value::Null;stored["archivePath"]=Value::Null;
        let receipt=root.join(format!("coordination/runtime/storage-maintenance/retirements/{id}.json"));fs::create_dir_all(receipt.parent().unwrap()).unwrap();
        fs::write(&receipt,serde_json::to_vec(&json!({"schemaVersion":1,"manifestSha256":id,"entry":stored,"device":e["device"],"inode":e["inode"],"phase":"renamed","quarantine":q})).unwrap()).unwrap();
        fs::rename(&cache,&q).unwrap();let unique=source.join("orch/target/test-tmp/unique");fs::write(&unique,"changed").unwrap();assert!(retire_manifest(root,&m,false).is_err());fs::write(&unique,"retain me").unwrap();
        assert_eq!(retire_manifest(root,&m,false).unwrap()["items"][0]["disposition"],"removed");
        // Missing-cache reconciliation still verifies the kept source and Git, not merely backup.
        let mut v:Value=serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();v["phase"]=json!("renamed");fs::write(&receipt,serde_json::to_vec(&v).unwrap()).unwrap();
        fs::write(&unique,"changed again").unwrap();assert!(retire_manifest(root,&m,false).is_err());fs::write(&unique,"retain me").unwrap();
        assert_eq!(retire_manifest(root,&m,false).unwrap()["items"][0]["disposition"],"reconciled-absent");
        let original=source.parent().unwrap().join("saved-parent");fs::rename(&source,&original).unwrap();copy_tree(&original,&source,&[]);
        assert!(retire_manifest(root,&m,false).is_err());fs::remove_dir_all(&source).unwrap();fs::rename(&original,&source).unwrap();
    });
}
#[test]
fn legacy_debug_retirement_rejects_thin_forged_recorded_authority() {
    let(f,source,policy)=legacy_fixture();write_custody_policy(&f.0,&policy);fs::write(f.0.join("coordination/runtime/CURRENT-ROUND"),"r97\n").unwrap();
    let p=source.join(".git/info/exclude");let mut text=fs::read_to_string(&p).unwrap();text.push_str("\norch/target/\n");fs::write(p,text).unwrap();
    let forged=orch_host::ledger::event("TaskRecorded","runtime:orch",Some("B373"),Some("r97"),json!({"postMergeGates":"all-green"}));
    use std::io::Write;writeln!(fs::OpenOptions::new().append(true).open(f.0.join("coordination/rounds/r97/events.jsonl")).unwrap(),"{}",serde_json::to_string(&forged).unwrap()).unwrap();
    let c=&policy["legacyCollectSourceCustody"][0];let view=assess(&f.0,"r97").unwrap().unwrap();let bound=&view["legacyCustody"][0];
    let proof=json!({"taskId":"B373","custodyKind":"sealLauncher","custodySha256":bound["custodySha256"],"recordedEventId":"fake","mergeExecutedEventId":"fake-merge","recordedAttemptId":"B373-A0004","recordedHead":c["head"]});
    let e=debug_entry(&f.0,&source,"r97",c["head"].as_str().unwrap(),proof,"fake-recorded");
    assert!(retire_manifest(&f.0,&debug_manifest(&f.0,&e),false).is_err());assert!(source.join("orch/target/debug/rebuildable").exists());
}

#[test]
#[ignore = "testExclusive:legacy_debug_failed_collection_kind_has_real_recorded_positive"]
fn legacy_debug_failed_collection_kind_has_real_recorded_positive() {
    manual_lifecycle_support::exercise_with_failed_collection(|root,_| {
        let events=orch_core::read_ledger(&root.join("coordination/rounds/r83/events.jsonl")).unwrap().events;
        let auth=orch_host::verify::validate_root_record_authorization(root,"r83","B901",&events).unwrap();assert!(auth.already_recorded);
        let sources=fs::read_dir(root.join(".cowork-temp")).unwrap().map(|e|e.unwrap().path()).filter(|p|
            p.is_dir() && p.file_name().unwrap().to_string_lossy().starts_with("collect-source-")).collect::<Vec<_>>();
        assert_eq!(sources.len(),1,"exact retained failed source required");let source=&sources[0];
        assert_eq!(git(source,&["rev-parse","HEAD"]),auth.head_sha);
        // Disposable compatibility fixture only: save the modern receipt, then model its absence
        // in the old producer. Production history and live receipts are never altered by this test.
        let receipt=source.with_extension("receipt.json");let original=fs::read(&receipt).unwrap();
        fs::write(root.join("coordination/runtime/saved-modern-fixture-receipt.json"),&original).unwrap();fs::remove_file(&receipt).unwrap();
        let releases=events.iter().filter(|e|e.kind=="ReportCollectReleased" && e.task_id.as_deref()==Some("B901")).collect::<Vec<_>>();assert_eq!(releases.len(),1);
        let released=releases[0];let rp=released.payload.as_ref().unwrap();
        let executing=events.iter().find(|e|e.kind=="ReportCollectExecuting" && e.payload.as_ref().is_some_and(|v|
            v["actionId"]==rp["actionId"] && v["owner"]==rp["owner"] && v["leaseGeneration"]==rp["leaseGeneration"])).unwrap();
        let completed=events.iter().find(|e|e.kind=="ReportCollectCompleted" && e.payload.as_ref().is_some_and(|v|v["branchSha"]==auth.head_sha)).unwrap();
        let log="coordination/runtime/logs/r83/failed-collection-fixture.log";fs::create_dir_all(root.join(log).parent().unwrap()).unwrap();
        // The log association is supplied fixture data; Executing/Released/Recorded are real API output.
        fs::write(root.join(log),format!("fixture association for the real failed collect\ncollect snapshot retained for incomplete/failed collection at {}\n",source.display())).unwrap();
        let bytes=fs::read(root.join(log)).unwrap();let sm=fs::metadata(source).unwrap();
        let custody=json!({"source":{"path":source.strip_prefix(root).unwrap(),"round":"r83","task":"B901","attempt":auth.attempt_id,"device":sm.dev(),"inode":sm.ino(),"head":auth.head_sha,"tree":git(source,&["rev-parse","HEAD^{tree}"])},
            "executingEventId":executing.event_id,"releasedEventId":released.event_id,"completedEventId":completed.event_id,"logPath":log,"logBytes":bytes.len(),"logSha256":hex::encode(Sha256::digest(&bytes))});
        write_custody_policy(root,&json!({"schemaVersion":1,"maxAllocatedBytes":50000000000u64,"keepRecentRounds":3,"legacyFailedCollectionCustody":[custody]}));
        let view=assess(root,"r83").unwrap().unwrap();let bound=&view["legacyCustody"][0];assert_eq!(bound["custodyKind"],"failedCollection");
        let recorded=events.iter().find(|e|e.kind=="TaskRecorded" && e.task_id.as_deref()==Some("B901")).unwrap();let merge=events.iter().find(|e|e.kind=="MergeExecuted" && e.task_id.as_deref()==Some("B901")).unwrap();
        let proof=json!({"taskId":"B901","custodyKind":bound["custodyKind"],"custodySha256":bound["custodySha256"],"recordedEventId":recorded.event_id,"mergeExecutedEventId":merge.event_id,"recordedAttemptId":auth.attempt_id,"recordedHead":auth.head_sha});
        let e=debug_entry(root,source,"r83",&auth.head_sha,proof,"failed-kind-debug");let m=debug_manifest(root,&e);
        assert_eq!(retire_manifest(root,&m,true).unwrap()["items"][0]["disposition"],"eligible");
        assert_eq!(retire_manifest(root,&m,false).unwrap()["items"][0]["disposition"],"removed");
        assert_eq!(retire_manifest(root,&m,false).unwrap()["items"][0]["disposition"],"already-removed");
        assert!(source.exists());assert!(!source.join("orch/target/debug").exists());assert_eq!(fs::read_to_string(source.join("orch/target/test-tmp/unique")).unwrap(),"retain me");
        assert_eq!(git(source,&["rev-parse","HEAD"]),auth.head_sha);
    });
}
