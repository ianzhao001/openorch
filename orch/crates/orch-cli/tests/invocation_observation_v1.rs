//! B352 real producer/readonly reader contract. Default and selfhost builds both
//! execute consult/standalone refresh+detail; no actual model is called.
//! Negatives: omit start/markers; use current config; infer roster or task;
//! accept unsafe/corrupt/changed result; cache absence/validity; create on read;
//! leak control tokens; unbound scans; lose control-split credential masking.
mod support;
use orch_host::observation::{ObservationReader, safe_observation_text};
use orch_host::consult::{run_consultation, ConsultArgs, ConsultOutcome};
use std::{fs, path::{Path,PathBuf}, process::{Command,Stdio}, time::{Duration,Instant}};
use std::os::unix::{fs::{PermissionsExt,symlink},process::CommandExt};

struct Fixture {root:PathBuf, head:String}
fn git(root:&Path,args:&[&str])->String {
    let out=support::fixture_git_command(root).args(args).output().unwrap();
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn wait_until(mut condition:impl FnMut()->bool)->bool {
    let end=Instant::now()+Duration::from_secs(10);
    while Instant::now()<end {if condition(){return true} std::thread::sleep(Duration::from_millis(20));}
    false
}
fn group_ended(pid:i32)->bool {
    unsafe extern "C" {fn killpg(group:i32,signal:i32)->i32;}
    (unsafe{killpg(pid,0)}) == -1 && std::io::Error::last_os_error().raw_os_error()==Some(3)
}
impl Fixture {
    fn new()->Self {
        let root=orch_host::util::test_scratch_dir("b350-observation-中文 space");
        fs::create_dir_all(root.join(".orch")).unwrap();
        fs::write(root.join(".gitignore"),".orch/\ncoordination/\n.cowork-temp/\n").unwrap();
        fs::write(root.join("question-B777.md"),"Read only. B999 / CONSULT / MANUAL are text, not task associations.\n").unwrap();
        let mut config=String::from("version: 1\nharnesses:\n");
        for alias in ["fast","slow"] {
            let path=root.join(alias);
            fs::write(&path,format!("#!/bin/sh\nset -eu\nprintf '%s' $$ > {alias}.started\nwhile [ ! -f release-{alias} ]; do /bin/sleep 0.02; done\nprintf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"FINAL: PASS {alias}\"}}'\n")).unwrap();
            fs::set_permissions(&path,fs::Permissions::from_mode(0o755)).unwrap();
            config.push_str(&format!("  {alias}:\n    driver: claude\n    executable: {}\n    enabled: true\n    defaults: {{model: initial-model}}\n    cwdPolicy: project-root\n",path.display()));
        }
        fs::write(root.join(".orch/harnesses.yaml"),config).unwrap();
        git(&root,&["init","-q"]);git(&root,&["add",".gitignore","question-B777.md","fast","slow"]);
        git(&root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","fixture"]);
        let head=git(&root,&["rev-parse","HEAD"]);Self{root,head}
    }
    fn start(&self)->Running {
        let work=self.root.clone();
        let handle=std::thread::spawn(move||run_consultation(&work,&ConsultArgs{question:"question-B777.md".into(),
            harnesses:vec!["fast".into(),"slow".into()],attachments:vec![],member_timeout_secs:Some(20),total_wall_secs:Some(25)}));
        Running{root:self.root.clone(),handle:Some(handle)}
    }
    fn complete(&self)->ConsultOutcome {let running=self.start();running.finish()}
    fn command(&self,args:&[&str])->Command {
        let mut command=Command::new(support::orch_bin());support::configure_fixture_git_env(&mut command,&[]);
        command.current_dir(&self.root).arg("--root").arg(&self.root).args(args);command
    }
    fn release(&self){for a in ["fast","slow"]{let _=fs::write(self.root.join(format!("release-{a}")),"release");}}
}
struct Running {root:PathBuf,handle:Option<std::thread::JoinHandle<anyhow::Result<ConsultOutcome>>>}
impl Running {
    fn finish(mut self)->ConsultOutcome {
        for a in ["fast","slow"]{fs::write(self.root.join(format!("release-{a}")),"release").unwrap();}
        self.handle.take().unwrap().join().unwrap().unwrap()
    }
}
impl Drop for Running {fn drop(&mut self){
    for a in ["fast","slow"]{let _=fs::write(self.root.join(format!("release-{a}")),"release");}
    if let Some(h)=self.handle.take(){let _=h.join();}
}}
impl Drop for Fixture {fn drop(&mut self){
    self.release();let mut safe=true;
    for a in ["fast","slow"] {
        if let Ok(s)=fs::read_to_string(self.root.join(format!("{a}.started"))) {
            if let Ok(pid)=s.parse::<i32>() {safe &= wait_until(||group_ended(pid));} else {safe=false;}
        }
    }
    if safe && !std::thread::panicking(){let _=fs::remove_dir_all(&self.root);}
}}

#[test]
fn all_members_are_visible_before_any_terminal_and_keep_captured_configuration() {
    let fixture=Fixture::new();let running=fixture.start();
    assert!(wait_until(||fixture.root.join("fast.started").exists()&&fixture.root.join("slow.started").exists()));
    let mut reader=ObservationReader::open(&fixture.root).unwrap();let snap=reader.refresh().unwrap();
    assert_eq!(snap.rows.len(),2);assert_eq!(snap.groups.len(),1);assert_eq!(snap.groups[0].total,Some(2));assert!(snap.groups[0].roster_complete);
    for row in &snap.rows {assert!(["prepared","entered","spawned"].contains(&row.phase.as_str()));assert_eq!(row.result,"none");assert!(row.task.is_none());assert_eq!(row.effective_tuple["model"],"initial-model");assert!(row.started_at.is_some());}
    fs::write(fixture.root.join(".orch/harnesses.yaml"),"invalid new configuration\n").unwrap();
    let changed=reader.refresh().unwrap();assert!(changed.rows.iter().all(|r|r.effective_tuple["model"]=="initial-model"));
    let done=running.finish();assert_eq!(done.members.len(),2);
}

#[test]
fn fast_final_and_slow_pending_coexist_and_details_verify_real_bytes() {
    let fixture=Fixture::new();let running=fixture.start();
    assert!(wait_until(||fixture.root.join("slow.started").exists()));
    let mut reader=ObservationReader::open(&fixture.root).unwrap();
    fs::write(fixture.root.join("release-fast"),"release").unwrap();
    assert!(wait_until(||reader.refresh().unwrap().rows.iter().any(|r|r.alias.as_deref()==Some("fast")&&r.result=="verified")));
    let snap=reader.refresh().unwrap();let fast=snap.rows.iter().find(|r|r.alias.as_deref()==Some("fast")).unwrap();
    let slow=snap.rows.iter().find(|r|r.alias.as_deref()==Some("slow")).unwrap();assert_ne!(slow.phase,"ended");assert_eq!(slow.result,"none");
    let detail=reader.detail(&fast.id).unwrap();assert_eq!(detail.row.result,"verified");assert_eq!(detail.text.as_deref(),Some("FINAL: PASS fast"));
    running.finish();
}

#[test]
fn historical_meta_can_supply_a_complete_roster_but_manifest_only_cannot() {
    let fixture=Fixture::new();let done=fixture.complete();let mut reader=ObservationReader::open(&fixture.root).unwrap();
    let original=reader.refresh().unwrap();assert_eq!(original.rows.len(),2);
    fs::remove_file(done.dir.join("start.json")).unwrap();
    let historical=reader.refresh().unwrap();assert_eq!(historical.groups[0].total,Some(2));assert!(historical.groups[0].roster_complete);
    assert!(historical.rows.iter().all(|r|r.started_at.is_none()&&r.task.is_none()));
    fs::remove_file(done.dir.join("meta.json")).unwrap();fs::remove_file(done.dir.join("fusion/1-slow.manifest.json")).unwrap();
    let partial=reader.refresh().unwrap();assert_eq!(partial.rows.len(),1);assert_eq!(partial.groups[0].total,None);assert!(!partial.groups[0].roster_complete);
}

#[test]
fn corrupt_member_and_changed_digest_do_not_keep_a_cached_valid_result() {
    let fixture=Fixture::new();let done=fixture.complete();let mut reader=ObservationReader::open(&fixture.root).unwrap();
    assert!(reader.refresh().unwrap().rows.iter().all(|r|r.result=="verified"));
    let p=done.dir.join("fusion/0-fast.manifest.json");let original=fs::read(&p).unwrap();let mut m:serde_json::Value=serde_json::from_slice(&original).unwrap();let bad="0".repeat(64);
    m["artifact"]["sha256"]=bad.clone().into();m["invocation"]["artifactSha256"]=bad.clone().into();m["channelFacts"]["terminal"]["finalTextSha256"]=bad.into();
    fs::write(&p,serde_json::to_vec_pretty(&m).unwrap()).unwrap();let changed=reader.refresh().unwrap();
    assert!(changed.rows.iter().any(|r|r.alias.as_deref()==Some("fast")&&r.result!="verified"));
    assert!(changed.rows.iter().any(|r|r.alias.as_deref()==Some("slow")&&r.result=="verified"));
    fs::write(&p,b"{broken").unwrap();let corrupt=reader.refresh().unwrap();assert!(corrupt.rows.iter().any(|r|r.alias.as_deref()==Some("slow")&&r.result=="verified"));assert!(!corrupt.diagnostics.is_empty());
}

#[test]
fn unsafe_answer_is_not_followed_and_reader_does_not_mutate_sources() {
    let fixture=Fixture::new();let done=fixture.complete();let outside=fixture.root.join("outside-secret");fs::write(&outside,"NEVER-READ-THIS-OUTSIDE-ANSWER").unwrap();
    let answer=done.dir.join("fusion/0-fast.md");fs::remove_file(&answer).unwrap();symlink(&outside,&answer).unwrap();
    let control=done.dir.join("start.json");let bytes=fs::read(&control).unwrap();let mode=fs::metadata(&control).unwrap().permissions().mode();
    let mut reader=ObservationReader::open(&fixture.root).unwrap();let snap=reader.refresh().unwrap();
    assert!(!serde_json::to_string(&snap).unwrap().contains("NEVER-READ-THIS"));
    assert!(snap.rows.iter().any(|r|r.alias.as_deref()==Some("slow")&&r.result=="verified"));
    assert_eq!(fs::read(control.clone()).unwrap(),bytes);assert_eq!(fs::metadata(control).unwrap().permissions().mode(),mode);
}

#[test]
fn empty_project_read_creates_no_runtime_or_configuration() {
    let fixture=Fixture::new();fs::remove_file(fixture.root.join(".orch/harnesses.yaml")).unwrap();
    assert!(!fixture.root.join("coordination").exists());let mut reader=ObservationReader::open(&fixture.root).unwrap();
    assert!(reader.refresh().unwrap().rows.is_empty());assert!(!fixture.root.join("coordination").exists());assert!(!fixture.root.join(".orch/harnesses.yaml").exists());
}

#[test]
fn live_writer_exit_leaves_only_last_observed_phase_without_fake_completion() {
    let fixture=Fixture::new();let mut command=fixture.command(&["consult","question-B777.md","--harness","fast","--harness","slow","--member-timeout-secs","20","--total-wall-secs","25"]);
    let mut writer=command.stdout(Stdio::null()).stderr(Stdio::null()).process_group(0).spawn().unwrap();
    let mut reader=ObservationReader::open(&fixture.root).unwrap();let ready=wait_until(||reader.refresh().unwrap().rows.iter().filter(|r|r.phase=="spawned").count()==2);
    let before=reader.refresh().unwrap();writer.kill().unwrap();writer.wait().unwrap();
    let after=reader.refresh().unwrap();fixture.release();
    assert!(ready);assert_eq!(after.rows.len(),2);
    for row in &after.rows {let prior=before.rows.iter().find(|r|r.id==row.id).unwrap();assert_eq!(row.phase,"spawned");assert_eq!(row.source_time,prior.source_time);assert_eq!(row.result,"none");assert!(row.task.is_none());}
}

#[test]
fn real_standalone_projection_retains_identity_and_never_exposes_control_tokens() {
    let fixture=Fixture::new();let exe=fixture.root.join("mock");fs::write(&exe,"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"text\",\"part\":{\"text\":\"owned\"}}'\n/bin/sleep 0.2\n").unwrap();fs::set_permissions(&exe,fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(fixture.root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  mock:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: native-initial, effort: high}}\n    cwdPolicy: project-root\n",exe.display())).unwrap();
    let out=fixture.command(&["wake","mock","--message-file","question-B777.md","--deadline-secs","10"]).output().unwrap();assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let text=String::from_utf8(out.stdout).unwrap();let wake=text.split("wakeId=").nth(1).unwrap().split_whitespace().next().unwrap();
    let path=fixture.root.join("coordination/runtime/supervisors").join(format!("{wake}.control.json"));let bytes=fs::read(&path).unwrap();let value:serde_json::Value=serde_json::from_slice(&bytes).unwrap();let token=value["token"].as_str().unwrap();let status=value["statusPath"].as_str().unwrap();let supervisor=value["supervisorPid"].as_u64().unwrap() as i32;
    assert!(wait_until(||fs::read(status).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|s|s["managedScopeTerminated"]==true)));
    assert!(wait_until(||{unsafe extern "C" {fn kill(pid:i32,signal:i32)->i32;} (unsafe{kill(supervisor,0)}) == -1 && std::io::Error::last_os_error().raw_os_error()==Some(3)}));
    fs::write(fixture.root.join(".orch/harnesses.yaml"),"invalid now").unwrap();let mode=fs::metadata(&path).unwrap().permissions().mode();
    let mut reader=ObservationReader::open(&fixture.root).unwrap();let snap=reader.refresh().unwrap();let row=snap.rows.iter().find(|r|r.alias.as_deref()==Some("mock")).unwrap();assert_eq!(row.effective_tuple["model"],"native-initial");assert!(row.task.is_none());assert!(!serde_json::to_string(&snap).unwrap().contains(token));
    assert_eq!(fs::read(&path).unwrap(),bytes);assert_eq!(fs::metadata(&path).unwrap().permissions().mode(),mode);
    fs::write(&path,format!("{{broken {token}")).unwrap();let bad=reader.refresh().unwrap();assert!(!serde_json::to_string(&bad).unwrap().contains(token));
}

#[test]
fn candidate_limits_are_reported_instead_of_silent_fullness() {
    let fixture=Fixture::new();let root=fixture.root.join("coordination/consultations");fs::create_dir_all(&root).unwrap();
    for n in 0..1026 {fs::create_dir(root.join(format!("{n:026}"))).unwrap();}
    let mut reader=ObservationReader::open(&fixture.root).unwrap();let snap=reader.refresh().unwrap();assert!(snap.truncated);assert!(!snap.diagnostics.is_empty());
}

#[test]
fn safe_text_uses_both_token_boundary_views_before_clipping() {
    for input in ["sk-\u{1b}abcdef", "Bearer to\u{7}ken", "API_KE\u{7}Y=abcdef", "x=1\nAPI_KEY=abcdef"] {
        let safe=safe_observation_text(input);assert!(!safe.contains("abcdef")&&!safe.contains("token"),"{safe}");assert!(!safe.chars().any(char::is_control));
    }
    let safe=safe_observation_text("界面 \u{1b}]52;c;AAAA\u{7} \u{009b}31m");assert!(!safe.chars().any(char::is_control));
    assert_eq!(safe_observation_text("中文 /path with spaces"),"中文 /path with spaces");
}

#[cfg(feature="selfhost")]
fn wake_event(f:&Fixture,round:&str,n:u32)->serde_json::Value {
    serde_json::json!({"eventId":format!("{n:026}"),"ts":"2026-01-01T00:00:00Z","actor":"runtime:orch","type":"WakeIssued","round":round,"taskId":"B999",
      "payload":{"wakeId":format!("{n:08x}-1111-4111-8111-111111111111"),"harness":"reviewer","driver":"opencode","action":"review","role":"review","attemptId":"B999-A0001","fixedHead":f.head,
      "method":"unified-channel-v1","observationSource":"opencode-json-v1","cwdSelection":"project-root","invocationCwd":f.root,"configDigest":"a".repeat(64),"requestDigest":"b".repeat(64),"attachmentManifestSha256":"c".repeat(64),"commandDigest":"d".repeat(64),"executableIdentityDigest":"e".repeat(64),
      "requestedTuple":{"model":"review-model","provider":null,"effort":null,"mode":null},"effectiveTuple":{"model":"review-model","provider":null,"effort":null,"mode":null}}})
}

#[cfg(feature="selfhost")]
#[test]
fn explicit_task_chain_is_required_and_old_review_cannot_mark_new_head_recorded() {
    let f=Fixture::new();let dir=f.root.join("coordination/rounds/r100");fs::create_dir_all(&dir).unwrap();fs::create_dir_all(f.root.join("coordination/runtime")).unwrap();fs::write(f.root.join("coordination/runtime/CURRENT-ROUND"),"r100\n").unwrap();
    let answer=b"review answer";fs::write(dir.join("review.md"),answer).unwrap();let output=Command::new("/usr/bin/shasum").args(["-a","256"]).arg(dir.join("review.md")).output().unwrap();assert!(output.status.success());let hash=String::from_utf8(output.stdout).unwrap().split_whitespace().next().unwrap().to_owned();
    let write=|recorded_head:&str|{
        let start=wake_event(&f,"r100",1);let wake=start["payload"]["wakeId"].as_str().unwrap();let mut binding=serde_json::Map::new();for key in ["configDigest","requestDigest","attachmentManifestSha256","commandDigest","executableIdentityDigest","requestedTuple","effectiveTuple","driver","harness","observationSource","invocationCwd","cwdSelection","fixedHead"] {binding.insert(key.into(),start["payload"][key].clone());}let mut ev=vec![start.clone()];
        let mut add=|n:u32,kind:&str,actor:&str,payload:serde_json::Value|ev.push(serde_json::json!({"eventId":format!("{n:026}"),"ts":"2026-01-01T00:00:01Z","type":kind,"actor":actor,"round":"r100","taskId":"B999","payload":payload}));
        add(2,"ReviewRequested","runtime:orch",serde_json::json!({"wakeId":wake,"harness":"reviewer","role":"review","attemptId":"B999-A0001","reviewedHead":f.head}));
        add(3,"ManagedWakeTerminated","runtime:orch",serde_json::json!({"wakeId":wake,"agent":"reviewer","state":"answered","turnEnded":true,"terminalSeen":true,"managedScopeTerminated":true,"channelBinding":binding}));
        add(4,"ReviewDelivered","runtime:orch",serde_json::json!({"wakeId":wake,"harness":"reviewer","attemptId":"B999-A0001","reviewedHead":f.head,"role":"review","verdict":"PASS","path":"coordination/rounds/r100/review.md","bytes":answer.len(),"sha256":hash,"requestEventId":format!("{:026}",2),"terminalEventId":format!("{:026}",3)}));
        add(5,"ReportCollectCompleted","runtime:orch",serde_json::json!({"attemptId":"B999-A0001","branchSha":recorded_head}));
        add(6,"VerdictIssued","verifier:root",serde_json::json!({"attemptId":"B999-A0001","headSha":recorded_head,"verdict":"PASS"}));
        add(7,"MergeStarted","runtime:orch",serde_json::json!({"attemptId":"B999-A0001","headSha":recorded_head,"verdictEventId":format!("{:026}",6),"collectCompletedEventId":format!("{:026}",5)}));
        add(8,"MergeExecuted","reviewer:orch-runtime",serde_json::json!({"mergeSha":recorded_head,"policy":"no-ff"}));
        add(9,"TaskRecorded","runtime:orch",serde_json::json!({"postMergeGates":"all-green"}));
        fs::write(dir.join("events.jsonl"),ev.into_iter().map(|e|format!("{e}\n")).collect::<String>()).unwrap();
    };
    write(&f.head);let mut reader=ObservationReader::open(&f.root).unwrap();let snap=reader.refresh().unwrap();let row=snap.rows.iter().find(|r|r.alias.as_deref()==Some("reviewer")).unwrap();assert_eq!(row.task.as_ref().unwrap().id,"B999");assert_eq!(row.task.as_ref().unwrap().state,"recorded");
    fs::write(f.root.join("next"),"new revision").unwrap();git(&f.root,&["add","next"]);git(&f.root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","next"]);let newer=git(&f.root,&["rev-parse","HEAD"]);
    write(&newer);let snap=reader.refresh().unwrap();let row=snap.rows.iter().find(|r|r.alias.as_deref()==Some("reviewer")).unwrap();assert_ne!(row.task.as_ref().unwrap().state,"recorded");
}

#[cfg(feature="selfhost")]
#[test]
fn numeric_round_window_keeps_current_within_eight_sources() {
    let f=Fixture::new();fs::create_dir_all(f.root.join("coordination/runtime")).unwrap();fs::write(f.root.join("coordination/runtime/CURRENT-ROUND"),"r1\n").unwrap();
    for n in [1,90,91,92,93,94,95,96,97,98,99,100] {let round=format!("r{n}");let dir=f.root.join("coordination/rounds").join(&round);fs::create_dir_all(&dir).unwrap();fs::write(dir.join("events.jsonl"),format!("{}\n",wake_event(&f,&round,n))).unwrap();}
    let mut reader=ObservationReader::open(&f.root).unwrap();let snap=reader.refresh().unwrap();let rounds=snap.rows.iter().filter_map(|r|r.task.as_ref().map(|t|t.round.clone())).collect::<std::collections::BTreeSet<_>>();
    assert!(rounds.contains("r100")&&rounds.contains("r1"),"{rounds:?}");assert!(rounds.len()<=8);assert!(!rounds.contains("r90"));
}
