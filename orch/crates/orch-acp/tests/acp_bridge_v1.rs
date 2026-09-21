//! B366 real fake-adapter processes over the production ACP client, never provider inference.
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
import sys,json
from pathlib import Path
root=Path(__file__).parent
kind=(root/'kind').read_text()
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
 if method=='initialize':result={'protocolVersion':2 if kind=='wrong-version' else 1,'agentCapabilities':{},'agentInfo':{'name':'OpenCode','version':'fixture-v1'}}
 elif method=='session/new':result={'sessionId':'s1','configOptions':options()}
 elif method=='session/set_config_option':
  q=req['params'];values[q['configId']]=q['value'];result={'configOptions':options()}
 elif method=='session/prompt':
  assert values=={'mode':'plan','model':'deepseek/deepseek-flash','effort':'max'}
  if kind=='tools-only':update={'sessionUpdate':'tool_call','toolCallId':'t1','title':'Read','status':'completed','content':[{'type':'content','content':{'type':'text','text':'FORGED'}}]}
  else:update={'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'verified answer'}}
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'s1','update':update}})
  if kind=='no-terminal':sys.exit(0)
  result={'stopReason':'end_turn'}
 else:
  if 'id' in req:emit({'jsonrpc':'2.0','id':req['id'],'error':{'code':-32601,'message':'unsupported'}})
  continue
 emit({'jsonrpc':'2.0','id':req['id'],'result':result})
"#).unwrap();fs::set_permissions(exe,fs::Permissions::from_mode(0o755)).unwrap();Self{_dir:dir,root}
 }
 fn request(&self)->AcpRequest {
  serde_json::from_value(json!({"version":1,"id":"bridge-case","requestDigest":"a".repeat(64),"target":"opencode","project":self.root,"executable":self.root.join("agent.py"),"adapter":null,"node":null,"agentVersion":"fixture-v1","provider":"deepseek","model":"deepseek-flash","effort":"max","mode":"plan","home":self.root.join("home"),"configDir":null,"isolationDir":self.root.join("private"),"prompt":"A finite question.","timeoutSecs":5})).unwrap()
 }
 fn prompted(&self)->bool {fs::read_to_string(self.root.join("calls")).unwrap_or_default().lines().any(|m|m=="session/prompt")}
}
#[test]
fn exact_readonly_pins_precede_prompt_and_native_completion() {
 let f=Fixture::new("good");let a=tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).unwrap();
 assert_eq!(a.text,"verified answer");assert_eq!(a.model,"deepseek-flash");assert_eq!(a.effort.as_deref(),Some("max"));assert_eq!(a.mode,"plan");assert_eq!(a.stop_reason,"end_turn");assert_eq!(a.native_exit_code,0);assert!(a.stdout_eof && a.stderr_eof);assert!(f.prompted());
}
#[test]
fn missing_setting_or_wrong_version_refuses_before_prompt() {
 for kind in ["missing-effort","wrong-version"] {
  let f=Fixture::new(kind);assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err());assert!(!f.prompted());
 }
}
#[test]
fn tool_content_or_eof_cannot_become_a_verified_answer() {
 for kind in ["tools-only","no-terminal"] {
  let f=Fixture::new(kind);assert!(tokio::runtime::Runtime::new().unwrap().block_on(consult(f.request())).is_err());
 }
}
