//! Actual SDK stdio and owned fake-model processes; never a real provider call.
use serde_json::{json,Value};
use std::{fs,io::{BufRead,BufReader,Write},os::unix::fs::PermissionsExt,path::{Path,PathBuf},process::{Child,ChildStdin,Command,Stdio},sync::mpsc::{self,Receiver},time::{Duration,Instant}};
struct Fixture { dir:tempfile::TempDir, root:PathBuf }
fn git(root:&Path,args:&[&str]) {
 let out=Command::new("/usr/bin/git").args(["-c","core.fsmonitor=false","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","-c","user.name=Fixture","-c","user.email=fixture@example.invalid"]).args(args).current_dir(root).output().unwrap();
 assert!(out.status.success(),"Git fixture failed: {}",String::from_utf8_lossy(&out.stderr));
}
impl Fixture {
 fn new()->Self {
  let dir=tempfile::tempdir().unwrap();let root=dir.path().join("project");fs::create_dir(&root).unwrap();
  git(&root,&["init","-q"]);git(&root,&["commit","--allow-empty","-qm","fixture"]);
  fs::create_dir(root.join(".orch")).unwrap();fs::write(root.join(".gitignore"),".orch/\n").unwrap();
  let exe=root.join("fake-client");fs::write(&exe,r#"#!/usr/bin/python3
import sys,json,time,os
from pathlib import Path
p=Path(__file__).parent
a=sys.argv
assert '--dangerously-skip-permissions' not in a
assert a[a.index('--permission-mode')+1]=='plan'
assert a[a.index('--tools')+1]=='Read,Glob,Grep'
assert a[a.index('--model')+1]=='fixture-flash'
assert a[a.index('--effort')+1]=='max'
f=os.open(str(p/'calls'),os.O_CREAT|os.O_APPEND|os.O_WRONLY,0o600);os.write(f,b'call\n');os.close(f)
if (p/'delay').exists():time.sleep(2 if (p/'delay').read_text()=='long' else .5)
print(json.dumps({'type':'system','subtype':'init','model':'fixture-flash'}))
print(json.dumps({'type':'result','subtype':'success','is_error':False,'result':'答卷 alpha'}))
"#).unwrap();fs::set_permissions(&exe,fs::Permissions::from_mode(0o755)).unwrap();
  fs::write(root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  fixture:\n    driver: claude\n    executable: {}\n    enabled: true\n    defaults: {{model: fixture-flash, effort: max}}\n    cwdPolicy: project-root\n",exe.display())).unwrap();
  Self{dir,root}
 }
 fn server(&self,root:&Path)->Server {
  let mut child=Command::new(env!("CARGO_BIN_EXE_orch-mcp")).args(["--project"]).arg(root).current_dir(root)
   .env_clear().env("HOME",self.dir.path()).env("PATH","/usr/bin:/bin").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
  let input=Some(child.stdin.take().unwrap());let stdout=child.stdout.take().unwrap();let (tx,rx)=mpsc::channel();
  std::thread::spawn(move ||{for line in BufReader::new(stdout).lines(){match line {Ok(line)=>{if tx.send(line).is_err(){break}},Err(_)=>break}}});
  Server{child,input,rx}
 }
 fn payload(&self,id:&str)->Value {json!({"project":".","requestKey":id,"question":"Read only evidence.","members":[{"id":"one","name":"One","instructions":"Consult only.","harness":"configured:fixture","fixed":{"model":"fixture-flash","effort":"max"}}]})}
 fn calls(&self)->usize {fs::read_to_string(self.root.join("calls")).unwrap_or_default().lines().count()}
}
struct Server { child:Child,input:Option<ChildStdin>,rx:Receiver<String> }
impl Server {
 fn send(&mut self,v:Value){let input=self.input.as_mut().unwrap();writeln!(input,"{v}").unwrap();input.flush().unwrap();}
 fn recv(&self)->Value {let line=self.rx.recv_timeout(Duration::from_secs(20)).expect("bounded MCP response");serde_json::from_str(&line).expect("stdout must contain only JSON protocol frames")}
 fn init(&mut self){self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"qualification","version":"1"}}}));let v=self.recv();assert!(v.get("error").is_none(),"{v}");assert_eq!(v["result"]["serverInfo"]["name"],"orch-mcp");self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));}
 fn call(&mut self,id:u32,name:&str,args:Value)->Value {self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}}));let v=self.recv();assert_eq!(v["id"],id);v}
 fn finish(&mut self)->std::process::ExitStatus {
  self.input.take();let start=Instant::now();loop {if let Some(status)=self.child.try_wait().unwrap(){return status;}assert!(start.elapsed()<Duration::from_secs(10),"server did not finish after EOF");std::thread::sleep(Duration::from_millis(20));}
 }
}
impl Drop for Server {fn drop(&mut self){if self.child.try_wait().ok().flatten().is_none(){let _=self.child.kill();let _=self.child.wait();}}}
fn result(v:&Value)->&Value {assert_ne!(v["result"]["isError"],true,"{v}");&v["result"]["structuredContent"]}
#[test]
fn handshake_exact_tools_no_inference_and_structured_refusals(){
 let f=Fixture::new();let mut s=f.server(&f.root);s.init();s.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));let v=s.recv();
 let mut names=v["result"]["tools"].as_array().unwrap().iter().map(|t|t["name"].as_str().unwrap()).collect::<Vec<_>>();names.sort();assert_eq!(names,["consult","get_run","list_harnesses","read_answer"]);
 let v=s.call(3,"list_harnesses",json!({"project":"."}));let h=result(&v)["harnesses"].as_array().unwrap().iter().find(|h|h["id"]=="configured:fixture").unwrap();assert_eq!(h["consult"]["available"],true);assert!(h.get("sources").is_none());assert!(h.get("executable").is_none());assert_eq!(f.calls(),0);
 let v=s.call(4,"list_harnesses",json!({"project":"/"}));assert_eq!(v["result"]["isError"],true);assert_eq!(v["result"]["structuredContent"]["error"]["code"],"project_not_allowed");
 let v=s.call(5,"get_run",json!({"project":".","runId":"x","waitMs":30001}));assert!(v.get("error").is_some()||v["result"]["isError"]==true);
 let v=s.call(6,"execute",json!({"command":"false"}));assert!(v.get("error").is_some()||v["result"]["isError"]==true);assert!(s.finish().success());
}
#[test]
fn duplicate_reservations_cross_process_pages_and_tampering(){
 let f=Fixture::new();fs::write(f.root.join("delay"),"yes").unwrap();let mut s=f.server(&f.root);s.init();
 for id in [10,11] {s.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"consult","arguments":f.payload("same")}}));}
 let a=s.recv();let b=s.recv();assert_eq!(result(&a)["id"],"same");assert_eq!(result(&b)["id"],"same");
 let mut reader=f.server(&f.root);reader.init();let v=reader.call(2,"get_run",json!({"project":".","runId":"same","waitMs":15000}));assert_eq!(result(&v)["phase"],"completed");assert!(result(&v).get("question").is_none());assert_eq!(f.calls(),1);
 let a=reader.call(3,"read_answer",json!({"project":".","runId":"same","memberId":"one","limit":3}));assert_eq!(result(&a)["text"],"答");
 let b=reader.call(4,"read_answer",json!({"project":".","runId":"same","memberId":"one","offset":3,"limit":65536,"sha256":result(&a)["sha256"]}));assert_eq!(result(&b)["text"],"卷 alpha");
 let wrong=reader.call(5,"read_answer",json!({"project":".","runId":"same","memberId":"one","offset":3,"sha256":"bad"}));assert_eq!(wrong["result"]["isError"],true);
 let state=f.root.join(".orch/fusion-runs/same/state.json");let mut value:Value=serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();value["phase"]=json!("completed");value["head"]=json!("tampered");fs::write(state,serde_json::to_vec(&value).unwrap()).unwrap();let v=reader.call(6,"get_run",json!({"project":".","runId":"same"}));assert_eq!(v["result"]["isError"],true);
 assert!(reader.finish().success());assert!(s.finish().success());
}
#[test]
fn eof_drains_admitted_run_and_sibling_cannot_read_it(){
 let f=Fixture::new();let sibling=f.dir.path().join("sibling");git(&f.root,&["worktree","add","--detach",sibling.to_str().unwrap()]);
 fs::write(f.root.join("delay"),"yes").unwrap();let mut s=f.server(&f.root);s.init();result(&s.call(2,"consult",f.payload("disconnect")));assert!(s.finish().success());assert_eq!(f.calls(),1);
 let mut other=f.server(&sibling);other.init();let v=other.call(2,"get_run",json!({"project":".","runId":"disconnect"}));assert_eq!(v["result"]["isError"],true);assert_eq!(v["result"]["structuredContent"]["error"]["code"],"run_project_mismatch");
 let v=other.call(3,"read_answer",json!({"project":".","runId":"disconnect","memberId":"one"}));assert_eq!(v["result"]["isError"],true);assert!(other.finish().success());
 let mut owner=f.server(&f.root);owner.init();let v=owner.call(2,"get_run",json!({"project":".","runId":"disconnect"}));assert_eq!(result(&v)["phase"],"completed");assert!(owner.finish().success());
}
#[test]
fn oversized_frame_and_incomplete_handshake_exit_without_inference(){
 let f=Fixture::new();let mut s=f.server(&f.root);let mut frame=vec![b' ';2*1024*1024+1];frame.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"clientInfo\":{\"name\":\"large\",\"version\":\"1\"}}}\n");
 let _=s.input.as_mut().unwrap().write_all(&frame);assert!(!s.finish().success());
 for line in s.rx.try_iter(){let value:Value=serde_json::from_str(&line).unwrap();assert!(value.get("result").is_none(),"oversized valid JSON frame must not initialize");}assert_eq!(f.calls(),0);
 let mut s=f.server(&f.root);s.input.as_mut().unwrap().write_all(b"{invalid\n").unwrap();assert!(!s.finish().success());assert_eq!(f.calls(),0);
}

#[test]
fn concurrent_gateway_preserves_both_reservations() {
 use orch_host::{fusion_run::FusionEngine,native_discovery::DiscoveryContext};
 use orch_mcp::service::Gateway;
 let f=Fixture::new();
 let context=DiscoveryContext{project:f.root.clone(),home:f.dir.path().to_path_buf(),search_path:vec!["/usr/bin".into(),"/bin".into()],query_timeout_ms:1000,allow_commands:false,overrides:Default::default(),include_platform_locations:false};
 let g=Gateway::with_engine(vec![f.root.clone()],FusionEngine::with_discovery_context(context)).unwrap();
 let p=f.payload("concurrent");let runtime=tokio::runtime::Runtime::new().unwrap();
 let (a,b)=runtime.block_on(async {tokio::join!(g.invoke("consult",p.clone()),g.invoke("consult",p))});
 assert!(a.is_ok() && b.is_ok(),"concurrent results: {a:?}; {b:?}");
 runtime.block_on(g.shutdown()).unwrap();assert_eq!(f.calls(),1);assert!(runtime.block_on(g.invoke("list_harnesses",json!({"project":"."}))).is_err());
}

#[test]
fn cancelled_wait_retains_reservation_and_sigterm_drains_job() {
 let f=Fixture::new();fs::write(f.root.join("delay"),"yes").unwrap();let mut s=f.server(&f.root);s.init();result(&s.call(2,"consult",f.payload("cancel")));
 s.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_run","arguments":{"project":".","runId":"cancel","waitMs":30000}}}));
 s.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"client stopped waiting"}}));
 let status=Command::new("/bin/kill").args(["-TERM",&s.child.id().to_string()]).status().unwrap();assert!(status.success());assert!(s.finish().success());
 let mut reader=f.server(&f.root);reader.init();let v=reader.call(2,"get_run",json!({"project":".","runId":"cancel","waitMs":15000}));assert_eq!(result(&v)["phase"],"completed");assert_eq!(f.calls(),1);assert!(reader.finish().success());
}
#[test]
fn simultaneous_clients_reuse_one_reservation_and_conflict_stays_explicit(){
 let f=Fixture::new();fs::write(f.root.join("delay"),"yes").unwrap();let mut a=f.server(&f.root);let mut b=f.server(&f.root);a.init();b.init();
 let req=json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"consult","arguments":f.payload("two-processes")}});a.send(req.clone());b.send(req);
 assert_eq!(result(&a.recv())["id"],"two-processes");assert_eq!(result(&b.recv())["id"],"two-processes");
 let mut p=f.payload("two-processes");p["question"]=json!("different");let v=b.call(3,"consult",p);assert_eq!(v["result"]["isError"],true);assert_eq!(v["result"]["structuredContent"]["error"]["code"],"request_id_conflict");
 assert!(a.finish().success());assert!(b.finish().success());assert_eq!(f.calls(),1);
}
#[test]
fn disconnected_owner_is_hold_without_automatic_replay(){
 let f=Fixture::new();let mut s=f.server(&f.root);s.init();result(&s.call(2,"consult",f.payload("abnormal")));let v=s.call(3,"get_run",json!({"project":".","runId":"abnormal","waitMs":15000}));assert_eq!(result(&v)["phase"],"completed");assert!(s.finish().success());
 // Model an interrupted evidence publication with no live owner; do not kill an
 // owned model and abandon it in a test. The common OS lock decides ownership.
 let state=f.root.join(".orch/fusion-runs/abnormal/state.json");let mut v:Value=serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();v["phase"]=json!("consulting");fs::write(state,serde_json::to_vec(&v).unwrap()).unwrap();
 let mut reader=f.server(&f.root);reader.init();let v=reader.call(2,"get_run",json!({"project":".","runId":"abnormal"}));assert_eq!(result(&v)["phase"],"hold");
 let v=reader.call(3,"consult",f.payload("abnormal"));assert_eq!(result(&v)["phase"],"hold");assert_eq!(f.calls(),1);assert!(reader.finish().success());
}

#[test]
fn cancellation_alone_releases_observation_slots_without_cancelling_job(){
 let f=Fixture::new();fs::write(f.root.join("delay"),"long").unwrap();let mut s=f.server(&f.root);s.init();result(&s.call(2,"consult",f.payload("slots")));
 for id in 3..11 {s.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"get_run","arguments":{"project":".","runId":"slots","waitMs":30000}}}));}
 std::thread::sleep(Duration::from_millis(150));
 for id in 3..11 {s.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}));}
 std::thread::sleep(Duration::from_millis(150));
 s.send(json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"list_harnesses","arguments":{"project":"."}}}));
 let start=Instant::now();loop {let v=s.recv();if v["id"]==20 {result(&v);break;}assert!(start.elapsed()<Duration::from_secs(1),"cancelled observation still occupied admission");}
 assert!(s.finish().success());assert_eq!(f.calls(),1);
}
#[test]
fn oversized_catalog_returns_bounded_explicit_error_without_models(){
 let f=Fixture::new();let exe=f.root.join("fake-client");let mut config=String::from("version: 1\nharnesses:\n");
 for i in 0..800 {config+=&format!("  fixture{i}:\n    driver: claude\n    executable: {}\n    enabled: true\n    defaults: {{model: {}, effort: max}}\n    cwdPolicy: project-root\n",exe.display(),"m".repeat(400));}
 fs::write(f.root.join(".orch/harnesses.yaml"),config).unwrap();let mut s=f.server(&f.root);s.init();let v=s.call(2,"list_harnesses",json!({"project":"."}));assert_eq!(v["result"]["isError"],true,"expected explicit response bound");assert_eq!(v["result"]["structuredContent"]["error"]["code"],"response_too_large");assert!(serde_json::to_vec(&v).unwrap().len()<2*1024*1024);assert_eq!(f.calls(),0);assert!(s.finish().success());
}

impl Fixture {
 fn registry_server(&self,registry:&Path,default_startup:bool)->Server {
  let mut command=Command::new(env!("CARGO_BIN_EXE_orch-mcp"));
  if !default_startup {command.arg("--projects-file").arg(registry);}
  let mut child=command.current_dir(self.dir.path()).env_clear().env("HOME",self.dir.path()).env("PATH","/usr/bin:/bin")
   .env("OPENORCH_CONFIG_DIR",registry.parent().unwrap()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
  let input=Some(child.stdin.take().unwrap());let stdout=child.stdout.take().unwrap();let(tx,rx)=mpsc::channel();
  std::thread::spawn(move||{for line in BufReader::new(stdout).lines(){match line{Ok(line)=>if tx.send(line).is_err(){break},Err(_)=>break}}});Server{child,input,rx}
 }
}
#[test]
fn registered_project_startup_is_explicit_and_does_not_authorize_cwd() {
 let f=Fixture::new();let directory=fs::canonicalize(f.dir.path()).unwrap().join("personal");
 orch_host::openorch_helper::attach_project(&f.root,&directory).unwrap();let registry=directory.join("projects.json");
 for default_startup in [false,true] {
  let mut server=f.registry_server(&registry,default_startup);server.init();
  let value=server.call(2,"list_harnesses",json!({"project":"."}));assert!(result(&value)["harnesses"].as_array().unwrap().iter().any(|h|h["id"]=="configured:fixture"));
  let value=server.call(3,"list_harnesses",json!({"project":f.dir.path()}));assert_eq!(value["result"]["isError"],true);
  assert!(server.finish().success());
 }
 let mut child=Command::new(env!("CARGO_BIN_EXE_orch-mcp")).arg("--project").arg(&f.root).arg("--projects-file").arg(&registry).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
 let mut input=child.stdin.take().unwrap();
 let _=writeln!(input,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"mixed-selector","version":"1"}}}));
 let _=writeln!(input,"{}",json!({"jsonrpc":"2.0","method":"notifications/initialized"}));drop(input);
 let output=child.wait_with_output().unwrap();assert!(!output.status.success());assert!(output.stdout.is_empty());assert_eq!(f.calls(),0);
}
#[test]
fn registered_project_startup_rejects_stale_identity_without_protocol_output() {
 let f=Fixture::new();let directory=fs::canonicalize(f.dir.path()).unwrap().join("personal");orch_host::openorch_helper::attach_project(&f.root,&directory).unwrap();
 let registry=directory.join("projects.json");let mut value:Value=serde_json::from_slice(&fs::read(&registry).unwrap()).unwrap();
 value["projects"][0]["inode"]=json!(value["projects"][0]["inode"].as_u64().unwrap()+1);fs::write(&registry,serde_json::to_vec(&value).unwrap()).unwrap();
 let output=Command::new(env!("CARGO_BIN_EXE_orch-mcp")).arg("--projects-file").arg(&registry).stdin(Stdio::null()).output().unwrap();
 assert!(!output.status.success());assert!(output.stdout.is_empty());assert_eq!(f.calls(),0);
}
