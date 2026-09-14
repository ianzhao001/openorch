//! B349: capture-local limits via the real prepare/render/preflight/run facade.
//! Semantic negatives: reinstate child RLIMIT; off-by-one or shared stream cap;
//! stop reading on overflow; infer EOF from group-empty; bypass consult veto;
//! stop draining after leader exit; lose original capture handle; remove docs.
//! Private helper/constructed-consult tests additionally prove fairness, write-error
//! discard and an independent acceptance veto with an otherwise clean native final.
#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, path::{Path,PathBuf}, process::Command, time::Instant};
use orch_host::channel::{capture_attachment_manifest_v1,preflight_invocation_v1,prepare_invocation,
    render_invocation_v1,run_preflighted_invocation_v1,ChannelExecution,InvocationAction,
    InvocationContextV1,InvocationRequest};
use orch_host::harness_config::parse_harness_config_snapshot;
const CAP: usize = 64*1024*1024;
fn git(root:&Path,args:&[&str])->String {
    let o=Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
    assert!(o.status.success(),"{}",String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
struct Fixture {root:PathBuf,head:String,exe:PathBuf}
impl Fixture {
    fn new(label:&str,body:&str)->Self {
        let root=orch_host::util::test_scratch_dir(&format!("b349-{label}"));
        fs::write(root.join("tracked"),"base\n").unwrap();
        git(&root,&["init","-q"]);git(&root,&["add","tracked"]);
        git(&root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","base"]);
        let head=git(&root,&["rev-parse","HEAD"]);let exe=root.join("provider");
        fs::write(&exe,format!("#!/bin/sh\nexec /usr/bin/python3 -I - <<'PY'\nimport os,sys,time,json,signal\nCAP=64*1024*1024\ndef emit(fd,n,byte=b'x'):\n while n:\n  b=byte*min(n,65536); k=os.write(fd,b); n-=k\n{body}\nPY\n")).unwrap();
        fs::set_permissions(&exe,fs::Permissions::from_mode(0o755)).unwrap();
        Self{root,head,exe}
    }
    fn run(&self,deadline:u64)->ChannelExecution {
        let config=format!("version: 1\nharnesses:\n  alpha:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: fixture, effort: high}}\n    consult: {{mode: pure}}\n    cwdPolicy: project-root\n",self.exe.display());
        let snapshot=parse_harness_config_snapshot(&self.root.join(".orch/harnesses.yaml"),&config).unwrap();
        let prepared=prepare_invocation(&snapshot,InvocationRequest{alias:"alpha".into(),action:InvocationAction::Consult,
            prompt:"fixture".into(),project_root:self.root.clone(),target_worktree:self.root.clone(),target_head:self.head.clone(),
            attachments:capture_attachment_manifest_v1(&[]).unwrap()}).unwrap();
        let rendered=render_invocation_v1(prepared,InvocationContextV1{action_id:"b349-fixture".into(),wake_id:"b349-fixture".into(),
            round:"r89".into(),task_id:"CONSULT".into(),attempt_id:"CONSULT-A0000".into(),review_output:None,
            orch_executable:self.exe.clone(),deadline_secs:deadline}).unwrap();
        run_preflighted_invocation_v1(preflight_invocation_v1(rendered).unwrap()).unwrap()
    }
}
fn release_escaped(root:&Path)->bool {
    let _=fs::write(root.join("release"),"release");
    let Some(pid)=fs::read_to_string(root.join("escaped-ready")).ok().and_then(|v|v.parse::<i32>().ok()).filter(|p|*p>1) else {return false};
    unsafe extern "C" {fn killpg(group:i32,signal:i32)->i32;}
    let until=Instant::now()+std::time::Duration::from_secs(5);
    loop {
        if unsafe{killpg(pid,0)} == -1 && std::io::Error::last_os_error().raw_os_error()==Some(3) {return true;}
        if Instant::now()>=until {return false;}
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
impl Drop for Fixture {fn drop(&mut self){
    if self.root.join("escaped-ready").exists() && !release_escaped(&self.root) {return;}
    // Preserve diagnostics and any not-yet-identified owned child on panic.
    if !std::thread::panicking(){let _=fs::remove_dir_all(&self.root);}
}}
fn complete(o:&ChannelExecution) {
    let f=o.facts();assert!(o.success(),"{f}");assert!(o.process_group_terminated,"{f}");
    assert_eq!(f["stdoutEofObserved"],true,"{f}");assert_eq!(f["stderrEofObserved"],true,"{f}");
}
#[test]
fn native_owned_files_can_grow_and_overwrite_above_capture_cap() {
    let f=Fixture::new("native-file",r#"p='owned-native-file'
with open(p,'wb') as out:
 out.seek(CAP+8192); out.write(b'a')
with open(p,'r+b') as out:
 out.seek(CAP+4096); out.write(b'b')
os.write(1,b'owned-file-ok\n'); os.write(2,b'diagnostic\n')"#);
    let o=f.run(15);complete(&o);assert_eq!(o.stdout,b"owned-file-ok\n");assert_eq!(o.stderr,b"diagnostic\n");
    assert_eq!(fs::metadata(f.root.join("owned-native-file")).unwrap().len(),CAP as u64+8193);
    assert_eq!(o.facts()["rawCaptureStable"],true);
    let existing=Fixture::new("existing-overwrite", "with open('existing-native','r+b') as out:\n out.seek(CAP+4096); out.write(b'z')\nos.write(1,b'overwrite-ok')");
    let file=fs::File::create(existing.root.join("existing-native")).unwrap();file.set_len(CAP as u64+8193).unwrap();drop(file);
    let overwritten=existing.run(15);complete(&overwritten);assert_eq!(overwritten.stdout,b"overwrite-ok");
    use std::os::unix::fs::FileExt;
    let file=fs::File::open(existing.root.join("existing-native")).unwrap();let mut byte=[0];file.read_exact_at(&mut byte,CAP as u64+4096).unwrap();
    assert_eq!(byte,[b'z']);assert_eq!(file.metadata().unwrap().len(),CAP as u64+8193);
}
#[test]
fn stdout_overflow_preserves_exact_prefix_without_killing_writer() {
    let f=Fixture::new("stdout-overflow","os.write(1,b'valid-looking-final\\n'); emit(1,CAP+100); os.write(2,b'writer-finished\\n')");
    let o=f.run(30);complete(&o);assert_eq!(o.stdout.len(),CAP);assert!(o.stdout.starts_with(b"valid-looking-final\n"));
    assert_eq!(o.stderr,b"writer-finished\n");let v=o.facts();assert_eq!(v["stdoutOverflow"],true);assert_eq!(v["stderrOverflow"],false);
    assert_eq!(v["rawCaptureStable"],false);assert_eq!(fs::metadata(o.stdout_capture_path.as_ref().unwrap()).unwrap().len(),CAP as u64);
}
#[test]
fn stderr_overflow_is_independent_of_valid_stdout() {
    let f=Fixture::new("stderr-overflow","os.write(1,b'answer\\n'); emit(2,CAP+1); os.write(1,b'finished\\n')");
    let o=f.run(30);complete(&o);assert_eq!(o.stdout,b"answer\nfinished\n");assert_eq!(o.stderr.len(),CAP);
    let v=o.facts();assert_eq!(v["stdoutOverflow"],false);assert_eq!(v["stderrOverflow"],true);assert_eq!(v["rawCaptureStable"],false);
}
#[test]
fn exact_cap_is_complete_and_both_streams_are_drained() {
    let f=Fixture::new("exact-cap","for i in range(CAP//65536):\n emit(1,65536,b'\\n'); emit(2,65536,b'b')");
    let o=f.run(30);complete(&o);assert_eq!(o.stdout,vec![b'\n';CAP]);assert_eq!(o.stderr,vec![b'b';CAP]);
    let v=o.facts();assert_eq!(v["stdoutOverflow"],false);assert_eq!(v["stderrOverflow"],false);assert_eq!(v["rawCaptureStable"],true);
}
#[test]
fn chatty_same_group_descendant_is_drained_after_leader_exit() {
    let f=Fixture::new("descendant",r#"pid=os.fork()
if pid:
 os._exit(0)
time.sleep(0.2)
emit(1,4*1024*1024,b't'); emit(2,3*1024*1024,b'e')
os._exit(0)"#);
    let o=f.run(15);complete(&o);assert_eq!(o.stdout,vec![b't';4*1024*1024]);assert_eq!(o.stderr,vec![b'e';3*1024*1024]);
    assert_eq!(o.facts()["rawCaptureStable"],true);
}
#[test]
fn escaped_writer_is_bounded_and_never_inferred_eof() {
    let f=Fixture::new("escaped",r#"pid=os.fork()
if pid:
 while not os.path.exists('escaped-ready'): time.sleep(0.005)
 os._exit(0)
os.setsid()
open('escaped-ready','w').write(str(os.getpid()))
until=time.monotonic()+10
while not os.path.exists('release') and time.monotonic()<until: time.sleep(0.01)
os.close(1); os.close(2)
open('escaped-done','w').write('closed')
os._exit(0)"#);
    let started=Instant::now();let o=f.run(6);let elapsed=started.elapsed();
    fs::write(f.root.join("release"),"release").unwrap();
    let until=Instant::now()+std::time::Duration::from_secs(5);
    while !f.root.join("escaped-done").exists() && Instant::now()<until {std::thread::sleep(std::time::Duration::from_millis(10));}
    let group_ended=release_escaped(&f.root);
    assert!(group_ended && f.root.join("escaped-done").exists(),"owned escaped fixture group did not close; preserve scene");
    assert!(elapsed.as_secs_f32()<5.5,"unbounded EOF wait: {elapsed:?}");assert!(o.process_group_terminated);
    let v=o.facts();assert_eq!(v["stdoutEofObserved"],false);assert_eq!(v["stderrEofObserved"],false);
    assert_eq!(v["rawCaptureStable"],false);assert!(!o.observation_errors.is_empty());
}
#[test]
fn replacement_path_does_not_replace_original_capture_bytes() {
    let f=Fixture::new("replacement",r#"import glob
for p in glob.glob('.cowork-temp/channel-capture/*'):
 os.unlink(p); os.symlink('/dev/null',p)
os.write(1,b'original-stdout\n'); os.write(2,b'original-stderr\n')"#);
    let o=f.run(15);complete(&o);assert_eq!(o.stdout,b"original-stdout\n");assert_eq!(o.stderr,b"original-stderr\n");
    assert_eq!(fs::read(o.stdout_capture_path.unwrap()).unwrap(),b"original-stdout\n");
    assert_eq!(fs::read(o.stderr_capture_path.unwrap()).unwrap(),b"original-stderr\n");
}
#[test]
fn public_capture_contract_documents_bounds_and_termination_limits() {
    let root=Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let source=fs::read_to_string(root.join("orch/crates/orch-host/src/channel.rs")).unwrap();
    let guide=fs::read_to_string(root.join("orch/docs/AI-MECHANICAL-GUIDE.md")).unwrap();
    assert!(source.contains("/// Capture completeness requires both actual pipe EOFs"));
    assert!(source.contains("/// Per-stream overflow"));
    for term in ["stdoutOverflow","stderrOverflow","stdoutEofObserved","stderrEofObserved","incomplete-capture","EPIPE","64 MiB"] {
        assert!(guide.contains(term),"missing substantive capture contract: {term}");
    }
}
