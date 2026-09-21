//! Supplemental producer/admission/lifecycle regressions using only owned fake adapters.
//! Mutate exact settings, protocol version, native terminal/body separation and readonly admission.
use orch_core::acp::AcpRequest;
use orch_acp::client::consult;
use serde_json::json;
use std::{fs,path::PathBuf,os::unix::fs::PermissionsExt};
struct Fixture { _dir:tempfile::TempDir, root:PathBuf }
impl Fixture {
 fn new(kind:&str)->Self {
  let dir=tempfile::tempdir().unwrap();let root=fs::canonicalize(dir.path()).unwrap();fs::create_dir(root.join("home")).unwrap();fs::create_dir(root.join("private")).unwrap();fs::write(root.join("kind"),kind).unwrap();
  let exe=root.join("agent.py");fs::write(&exe,r#"#!/usr/bin/python3
import sys,json,os
from pathlib import Path
root=Path(__file__).parent
kind=(root/'kind').read_text()
if 'debug' in sys.argv:
 rules=[{'permission':'*','pattern':'*','action':'deny'},{'permission':'external_directory','pattern':'/owned/*','action':'allow'}]
 if kind=='unsafe-policy':rules.append({'permission':'edit','pattern':'*','action':'allow'})
 if kind=='old-global-allow':rules.insert(0,{'permission':'bash','pattern':'*','action':'allow'})
 print(json.dumps({'name':'plan','permission':rules} if kind!='bad-policy' else {}));sys.exit(0)
if os.environ.get('OPENCODE_CONFIG_CONTENT'):
 cfg=json.loads(os.environ['OPENCODE_CONFIG_CONTENT']);assert cfg['permission']=='deny' and cfg['agent']['plan']['permission']=='deny';assert cfg['compaction']=={'auto':False,'prune':False};assert cfg['small_model']==cfg['model']
 (root/'tool-policy').write_text('deny')
(root/'group').write_text(json.dumps([os.getpgrp(),os.getpgid(os.getppid())]))
values={'mode':'build','model':'other/default','effort':'low'}
def emit(v):print(json.dumps(v),flush=True)
def options():
 out=[{'id':'model','name':'Model','type':'select','currentValue':values['model'],'options':[{'value':'deepseek/deepseek-flash','name':'Flash'}]}, {'id':'mode','name':'Mode','type':'select','currentValue':values['mode'],'options':[{'value':'plan','name':'Plan'}]}]
 if kind!='missing-effort':out.append({'id':'effort','name':'Effort','type':'select','currentValue':values['effort'],'options':[{'value':'max','name':'Max'}]})
 return out
for line in sys.stdin:
 req=json.loads(line);method=req.get('method')
 if method is None:continue
 with (root/'calls').open('a') as f:f.write(method+'\n')
 if method=='initialize':result={'protocolVersion':2 if kind=='wrong-version' else 1,'agentCapabilities':{},'agentInfo':{'name':'WrongAgent' if kind=='wrong-agent' else 'OpenCode','version':'wrong' if kind=='wrong-agent-version' else 'fixture-v1'}}
 elif method=='session/new':result={'sessionId':'s1','configOptions':options()}
 elif method=='session/set_config_option':
  q=req['params'];values[q['configId']]='low' if kind=='bad-readback' and q['configId']=='effort' else q['value'];result={'configOptions':options()}
 elif method=='session/prompt':
  assert values=={'mode':'plan','model':'deepseek/deepseek-flash','effort':'max'}
  if kind=='permission':
   emit({'jsonrpc':'2.0','id':900,'method':'session/request_permission','params':{'sessionId':'s1','toolCall':{'toolCallId':'t1','title':'Delegate','status':'pending'},'options':[{'optionId':'allow','name':'Allow','kind':'allow_once'}]}})
   response=json.loads(sys.stdin.readline());assert response['result']['outcome']['outcome']=='cancelled';(root/'denied').write_text('yes')
  if kind=='wrong-session':
   emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'wrong','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'FORGED'}}}})
  if kind=='changed-pins':
   values['effort']='low';emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'s1','update':{'sessionUpdate':'config_option_update','configOptions':options()}}})
  if kind=='tools-only':update={'sessionUpdate':'tool_call','toolCallId':'t1','title':'Read','status':'completed','content':[{'type':'content','content':{'type':'text','text':'FORGED'}}]}
  else:update={'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'verified answer'}}
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'s1','update':update}})
  if kind=='no-terminal':sys.exit(0)
  result={'stopReason':'end_turn'}
 else:
  if 'id' in req:emit({'jsonrpc':'2.0','id':req['id'],'error':{'code':-32601,'message':'unsupported'}})
  continue
 emit({'jsonrpc':'2.0','id':req['id'],'result':result})
 if method=='session/prompt' and kind=='late-text':
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'s1','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'LATE'}}}})
if kind=='bad-exit':sys.exit(7)
"#).unwrap();fs::set_permissions(exe,fs::Permissions::from_mode(0o755)).unwrap();Self{_dir:dir,root}
 }
 fn request(&self)->AcpRequest {
  serde_json::from_value(json!({"version":1,"id":"bridge-case","requestDigest":"a".repeat(64),"target":"opencode","project":self.root,"executable":self.root.join("agent.py"),"adapter":null,"node":null,"agentVersion":"fixture-v1","provider":"deepseek","model":"deepseek-flash","effort":"max","mode":"plan","home":self.root.join("home"),"configDir":null,"isolationDir":self.root.join("private"),"prompt":"A finite question.","timeoutSecs":5})).unwrap()
 }
 fn prompted(&self)->bool {fs::read_to_string(self.root.join("calls")).unwrap_or_default().lines().any(|m|m=="session/prompt")}
}
#[test]
fn wrong_agent_and_version_never_prompt() {
 for kind in ["wrong-agent","wrong-agent-version"] {let f=Fixture::new(kind);assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err(),"{kind}");assert!(!f.prompted());}
}
#[test]
fn rejected_readback_never_prompts() {
 let f=Fixture::new("bad-readback");assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err());assert!(!f.prompted());
}
#[test]
fn mismatched_session_or_changed_pin_is_not_an_answer() {
 for kind in ["wrong-session","changed-pins"] {let f=Fixture::new(kind);assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err(),"{kind}");}
}
#[test]
fn permission_is_denied_without_delegation() {
 let f=Fixture::new("permission");assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err());assert_eq!(fs::read_to_string(f.root.join("denied")).unwrap(),"yes");
}
#[test]
fn native_failure_or_late_text_cannot_be_completed() {
 for kind in ["bad-exit","late-text"] {let f=Fixture::new(kind);assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err(),"{kind}");}
}
#[test]
fn adapter_inherits_existing_process_group() {
 let f=Fixture::new("good");tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).unwrap();let groups:Vec<u64>=serde_json::from_slice(&fs::read(f.root.join("group")).unwrap()).unwrap();assert_eq!(groups[0],groups[1]);assert_eq!(fs::read_to_string(f.root.join("tool-policy")).unwrap(),"deny");
}
#[test]
fn unsafe_mode_and_environment_payload_are_rejected() {
 let f=Fixture::new("good");let mut request=f.request();request.mode="auto".into();assert!(request.validate().is_err());assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(request)).is_err());assert!(!f.prompted());
 let mut wrong=f.request();wrong.adapter=Some(f.root.join("other-agent"));assert!(wrong.validate().is_err());
 let mut value=serde_json::to_value(f.request()).unwrap();value["env"]=json!({"PATH":"/untrusted"});assert!(serde_json::from_value::<AcpRequest>(value).is_err());
}
#[test]
fn completed_envelope_still_binds_request_and_terminal() {
 let f=Fixture::new("good");let request=f.request();let a=tokio::runtime::Runtime::new().unwrap().block_on(consult(request.clone())).unwrap();
 let mut bad=a.clone();bad.request_digest="b".repeat(64);assert!(bad.validate(&request).is_err());let mut bad=a.clone();bad.model="deepseek-pro".into();assert!(bad.validate(&request).is_err());let mut bad=a;bad.stdout_eof=false;assert!(bad.validate(&request).is_err());
}

#[test]
fn bridge_file_digest_and_symlink_admission_precede_native_start() {
 use sha2::{Digest,Sha256};
 use std::process::Command;
 let f=Fixture::new("good");let path=f.root.join("request.json");
 let bytes=serde_json::to_vec(&f.request()).unwrap();fs::write(&path,&bytes).unwrap();
 let sha=hex::encode(Sha256::digest(&bytes));
 let invoke=|p:&std::path::Path,h:&str|Command::new(env!("CARGO_BIN_EXE_orch-acp")).args(["--request-file",p.to_str().unwrap(),"--sha256",h]).output().unwrap();
 let bad=invoke(&path,&"0".repeat(64));assert!(!bad.status.success());assert!(bad.stdout.is_empty());assert!(!f.prompted());
 let link=f.root.join("linked.json");std::os::unix::fs::symlink(&path,&link).unwrap();let bad=invoke(&link,&sha);assert!(!bad.status.success());assert!(bad.stdout.is_empty());assert!(!f.prompted());
 let good=invoke(&path,&sha);assert!(good.status.success(),"{}",String::from_utf8_lossy(&good.stderr));
 let envelope:orch_core::acp::AcpEnvelope=serde_json::from_slice(&good.stdout).unwrap();
 match envelope {orch_core::acp::AcpEnvelope::Completed{answer}=>answer.validate(&f.request()).unwrap(),_=>panic!("expected completed")};assert!(f.prompted());
}

#[test]
fn claude_picker_alias_is_not_actual_model_evidence() {
 for actual in ["deepseek-flash","opus","deepseek-pro"] {
  let f=Fixture::new("good");let exe=f.root.join("agent.py");
  let mut source=fs::read_to_string(&exe).unwrap().replace("'OpenCode'","'@agentclientprotocol/claude-agent-acp'").replace("deepseek/deepseek-flash","opus").replace("'name':'Flash'","'name':'deepseek-flash'");
  let raw=format!("emit({{ 'jsonrpc':'2.0','method':'_claude/sdkMessage','params':{{'sessionId':'s1','message':{{'type':'assistant','message':{{'model':'{actual}'}}}}}} }})\n  emit({{'jsonrpc':'2.0','method':'_claude/sdkMessage','params':{{'sessionId':'s1','message':{{'type':'result','subtype':'success','is_error':False}}}}}})\n  result={{'stopReason':'end_turn'}}");
  source=source.replace("result={'stopReason':'end_turn'}",&raw);fs::write(&exe,source).unwrap();
  let mut request=f.request();request.target=orch_core::acp::AcpTarget::Claude;request.adapter=Some(exe);request.provider_url=Some("https://api.deepseek.com/anthropic".into());
  let result=tokio::runtime::Runtime::new().unwrap().block_on(consult(request));
  assert_eq!(result.is_ok(),actual=="deepseek-flash","actual={actual}: {result:?}");
 }
}

#[test]
fn bounded_deadline_and_missing_terminal_never_return_partial_text() {
 for kind in ["no-terminal","hang"] {
  let f=Fixture::new(kind);if kind=="hang" {let exe=f.root.join("agent.py");let source=fs::read_to_string(&exe).unwrap().replace("result={'stopReason':'end_turn'}","import time; time.sleep(30)\n  result={'stopReason':'end_turn'}");fs::write(exe,source).unwrap();}
  let mut request=f.request();request.timeout_secs=4;let start=std::time::Instant::now();
  let error=tokio::runtime::Runtime::new().unwrap().block_on(consult(request)).unwrap_err();
  assert!(start.elapsed()<std::time::Duration::from_secs(8));assert!(error.prompt_sent);assert!(f.prompted());
 }
}

#[test]
fn effective_native_tool_allow_is_rejected_before_prompt() {
 for kind in ["good","unsafe-policy","old-global-allow","bad-policy"] {
  let f=Fixture::new(kind);let result=tokio::runtime::Runtime::new().unwrap().block_on(orch_acp::client::consult_verified(f.request()));
  let allowed=matches!(kind,"good"|"old-global-allow");
  assert_eq!(result.is_ok(),allowed,"{kind}: {result:?}");assert_eq!(f.prompted(),allowed);
  if let Err(failure)=result {
   assert!(!failure.prompt_sent);assert!(failure.code.starts_with("acp_native_policy"));
   use sha2::{Digest,Sha256};
   let bytes=serde_json::to_vec(&f.request()).unwrap();let path=f.root.join("policy-request.json");fs::write(&path,&bytes).unwrap();
   let output=std::process::Command::new(env!("CARGO_BIN_EXE_orch-acp")).arg("--request-file").arg(&path).arg("--sha256").arg(hex::encode(Sha256::digest(&bytes))).output().unwrap();
   assert!(!output.status.success());let envelope:orch_core::acp::AcpEnvelope=serde_json::from_slice(&output.stdout).unwrap();
   match envelope {orch_core::acp::AcpEnvelope::Rejected{failure}=>assert!(!failure.prompt_sent),_=>panic!("production bridge bypassed policy")};assert!(!f.prompted());
  }
 }
}
