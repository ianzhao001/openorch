//! Real default-feature observations, negative source mutations and read-only proofs.
use orch_host::{consult::{run_consultation,ConsultArgs},observation::{ObservationReader,safe_observation_text}};
use std::{fs,path::{Path,PathBuf},process::Command,os::unix::fs::PermissionsExt};
use serde_json::Value;
struct Fixture{root:PathBuf}
impl Fixture {
 fn new()->Self{
  let root=orch_host::util::test_scratch_dir("observation-default");fs::create_dir_all(root.join(".orch")).unwrap();
  fs::write(root.join(".gitignore"),".orch/\ncoordination/\n.cowork-temp/\n").unwrap();
  fs::write(root.join("question"),"Hello 中文").unwrap();let exe=root.join("provider");fs::write(&exe,"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ANSWER\"}'\n").unwrap();fs::set_permissions(&exe,fs::Permissions::from_mode(0o755)).unwrap();
  fs::write(root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  one:\n    driver: claude\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",exe.display())).unwrap();
  for args in [vec!["init","-q"],vec!["add","question"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","base"]]{assert!(Command::new("git").arg("-C").arg(&root).args(args).status().unwrap().success())}
  Self{root}
 }
 fn run(&self)->PathBuf{run_consultation(&self.root,&ConsultArgs{question:"question".into(),harnesses:vec!["one".into()],member_timeout_secs:Some(5),total_wall_secs:Some(10),..Default::default()}).unwrap().dir}
}
impl Drop for Fixture{fn drop(&mut self){if !std::thread::panicking(){let _=fs::remove_dir_all(&self.root);}}}
fn rewrite(path:&Path,v:&Value){fs::write(path,serde_json::to_vec(v).unwrap()).unwrap();}
#[test]
fn default_api_executes_refresh_detail_and_rejects_independent_terminal_contradictions(){
 let f=Fixture::new();let dir=f.run();let p=dir.join("fusion/0-one.manifest.json");let original:Value=serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();let mut r=ObservationReader::open(&f.root).unwrap();let snap=r.refresh().unwrap();assert_eq!(snap.rows[0].result,"verified");assert_eq!(r.detail(&snap.rows[0].id).unwrap().text.as_deref(),Some("ANSWER"));
 for (key,value)in [("status",Value::from("unknown")),("turnEnded",Value::from(false)),("mechanicalTerminalAbsent",Value::from(true)),("managedScopeTerminated",Value::from(false))]{let mut v=original.clone();v["channelFacts"]["terminal"][key]=value;rewrite(&p,&v);assert_ne!(r.refresh().unwrap().rows[0].result,"verified","{key}");rewrite(&p,&original);assert_eq!(r.refresh().unwrap().rows[0].result,"verified");}
}
#[test]
fn exact_start_identity_and_historical_membership_are_required(){
 let f=Fixture::new();let dir=f.run();let p=dir.join("start.json");let original:Value=serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();let mut r=ObservationReader::open(&f.root).unwrap();
 for key in ["driver","action","commandDigest","executableIdentityDigest"]{let mut v=original.clone();v["members"][0]["facts"][key]="different".into();rewrite(&p,&v);assert_ne!(r.refresh().unwrap().rows[0].result,"verified","{key}");}rewrite(&p,&original);
 fs::remove_file(p).unwrap();let meta=dir.join("meta.json");let mut v:Value=serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();v["membership"]["harnesses"][0]="other".into();rewrite(&meta,&v);let snap=r.refresh().unwrap();assert!(!snap.groups[0].roster_complete);assert_eq!(snap.groups[0].total,None);
}
#[test]
fn unknown_version_permissions_and_oversized_body_degrade_without_partial_verification(){
 let f=Fixture::new();let dir=f.run();let p=dir.join("fusion/0-one.manifest.json");let original=fs::read(&p).unwrap();let mut v:Value=serde_json::from_slice(&original).unwrap();v["schemaVersion"]=999.into();rewrite(&p,&v);let mut r=ObservationReader::open(&f.root).unwrap();let snap=r.refresh().unwrap();assert_ne!(snap.rows[0].result,"verified");assert!(!snap.diagnostics.is_empty());fs::write(&p,&original).unwrap();
 fs::set_permissions(&p,fs::Permissions::from_mode(0o666)).unwrap();assert_ne!(r.refresh().unwrap().rows[0].result,"verified");fs::set_permissions(&p,fs::Permissions::from_mode(0o600)).unwrap();
 fs::write(dir.join("fusion/0-one.md"),vec![b'x';1024*1024+1]).unwrap();assert_eq!(r.refresh().unwrap().rows[0].result,"too-large");
}
#[test]
fn fifo_record_without_writer_is_never_opened_blocking(){
 let f=Fixture::new();let dir=f.run();let p=dir.join("start.json");fs::remove_file(&p).unwrap();assert!(Command::new("mkfifo").arg(&p).status().unwrap().success());
 // Release a regression's blocking read after a bound so the test cannot hang.
 let other=p.clone();let rescue=std::thread::spawn(move||{std::thread::sleep(std::time::Duration::from_secs(2));let _=fs::OpenOptions::new().write(true).custom_flags(nonblock()).open(other);});
 let start=std::time::Instant::now();let mut r=ObservationReader::open(&f.root).unwrap();let snap=r.refresh().unwrap();assert!(start.elapsed()<std::time::Duration::from_secs(1));assert!(!snap.diagnostics.is_empty());rescue.join().unwrap();
}
use std::os::unix::fs::OpenOptionsExt;
#[cfg(target_os="macos")]fn nonblock()->i32{4}
#[cfg(not(target_os="macos"))]fn nonblock()->i32{0x800}
fn bytes_tree(root:&Path)->std::collections::BTreeMap<PathBuf,Vec<u8>>{let mut result=std::collections::BTreeMap::new();let mut dirs=vec![root.to_path_buf()];while let Some(dir)=dirs.pop(){for e in fs::read_dir(dir).unwrap().flatten(){if e.file_type().unwrap().is_dir(){dirs.push(e.path())}else if e.file_type().unwrap().is_file(){result.insert(e.path(),fs::read(e.path()).unwrap());}}}result}
#[test]
fn refresh_and_detail_leave_every_project_byte_unchanged(){let f=Fixture::new();f.run();let before=bytes_tree(&f.root);let mut r=ObservationReader::open(&f.root).unwrap();for _ in 0..3{let s=r.refresh().unwrap();r.detail(&s.rows[0].id).unwrap();}drop(r);assert_eq!(before,bytes_tree(&f.root));}
#[test]
fn controls_and_ansi_split_credentials_are_never_displayed(){for value in ["sk-\x1b[0mSECRET", "Bearer se\x1b[1mcret", "API_KEY=abc\n", "x=1\nAPI_KEY=abc"]{assert_eq!(safe_observation_text(value),"[redacted]");}assert!(!safe_observation_text("中文\u{009b}\x1b]52;c;AAA\x07").chars().any(char::is_control));let d=orch_host::consult::ChannelDiagnostic::provider_failure(Some("protocol"),Some("failed at /Users/private/secret"),None);assert_eq!(d.reason.as_deref(),Some("[redacted path]"));}
#[test]
fn public_observation_contract_has_substantive_docs(){let guide=include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");for text in ["phaseObservationV1","start.json","ObservationReader","32 MiB","8 MiB","last observed"]{assert!(guide.contains(text),"missing {text}");}}

#[test]
fn rejected_empty_and_timed_out_members_are_observed_without_success(){
 for case in ["rejected","empty","timeout"] {
  let f=Fixture::new();let args=ConsultArgs{question:"question".into(),harnesses:vec!["one".into()],member_timeout_secs:Some(1),total_wall_secs:Some(2),..Default::default()};
  if case=="rejected"{let path=f.root.join(".orch/harnesses.yaml");let text=fs::read_to_string(&path).unwrap().replace("enabled: true","enabled: false");fs::write(path,text).unwrap();}
  else {fs::write(f.root.join("provider"),if case=="empty"{"#!/bin/sh\nexit 0\n"}else{"#!/bin/sh\n/bin/sleep 10\n"}).unwrap();}
  run_consultation(&f.root,&args).unwrap();let mut reader=ObservationReader::open(&f.root).unwrap();let snapshot=reader.refresh().unwrap();assert_eq!(snapshot.rows.len(),1);assert_ne!(snapshot.rows[0].result,"verified");assert_eq!(snapshot.groups[0].total,Some(1));
 }
}

#[test]
fn derived_capture_stability_cannot_override_contradictory_raw_facts(){
 let f=Fixture::new();let dir=f.run();let path=dir.join("fusion/0-one.manifest.json");let original:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();let mut r=ObservationReader::open(&f.root).unwrap();
 for(key,value)in [("processGroupTerminated",Value::from(false)),("stdoutEofObserved",Value::from(false)),("stderrEofObserved",Value::from(false)),("stdoutOverflow",Value::from(true)),("stderrOverflow",Value::from(true)),("observationErrors",serde_json::json!(["error"]))]{let mut v=original.clone();v["channelFacts"]["execution"][key]=value;rewrite(&path,&v);assert_ne!(r.refresh().unwrap().rows[0].result,"verified","{key}");}
 rewrite(&path,&original);assert_eq!(r.refresh().unwrap().rows[0].result,"verified");
}

#[test]
fn legacy_all_missing_capture_facts_are_neutral_invalid_and_unreadable(){
 let f=Fixture::new();let dir=f.run();let path=dir.join("fusion/0-one.manifest.json");let original:Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();let mut legacy=original.clone();let execution=legacy["channelFacts"]["execution"].as_object_mut().unwrap();
 for key in ["stdoutEofObserved","stderrEofObserved","stdoutOverflow","stderrOverflow"]{execution.remove(key);}
 rewrite(&path,&legacy);let mut reader=ObservationReader::open(&f.root).unwrap();let snapshot=reader.refresh().unwrap();let row=&snapshot.rows[0];assert_eq!(row.result,"invalid");assert_eq!(row.channel_diagnostic().unwrap().code,orch_host::consult::DiagnosticCode::CaptureEvidenceMissing);assert!(reader.detail(&row.id).unwrap().text.is_none());
 let mut partial=original.clone();partial["channelFacts"]["execution"].as_object_mut().unwrap().remove("stdoutEofObserved");rewrite(&path,&partial);let snapshot=reader.refresh().unwrap();assert_eq!(snapshot.rows[0].result,"invalid");assert_eq!(snapshot.rows[0].channel_diagnostic().unwrap().code,orch_host::consult::DiagnosticCode::ProviderFailure);
}
