//! B347: immutable deterministic DSH session-set contract; no live models.
//! Real wrapper functions/Node helper, real owned filesystem and zstd, fixture SDK.
//! Existing function names and final_candidate_set_error are contract carriers.
//! No sleep-race oracle, production injection hook, or public decoder override.
//! Mutation classes (individual subcases; exact restoration and full seed green):
//! M1 skip bound re-enumeration/rebind; M2 omit dirty-final gate or change status precedence;
//! M3 omit raw/native publish crosschecks; M4 remove pre-link/post-link checks;
//! M5 allow pending capture under uncertainty; M6 forget disappeared candidates;
//! M7 reader literal reintroduction and durationMs schema removal (existing identity suite).
//! Two initial controls pass; ten deterministic behavior cases begin red.
#![cfg(all(unix, feature = "selfhost"))]
use std::{path::Path, process::Command};
fn scenario(name: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let output = Command::new("python3").args(["-c", PYTHON, root.to_str().unwrap(), name, env!("CARGO_BIN_EXE_orch")]).output().expect("start model-free DSH contract fixture");
    assert!(output.status.success(), "B347 {name} failed\nstdout={}\nstderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
#[test]
fn late_valid_session_never_inherits_first_binding() { scenario("late-valid"); }

#[test]
fn unfinished_candidate_is_revisited_and_held_at_end() { scenario("incomplete-lifecycle"); }

#[test]
fn disappeared_unresolved_candidate_remains_uncertain() { scenario("remember-disappeared"); }

#[test]
fn final_inventory_requires_bounded_stability() { scenario("stable-final"); }

#[test]
fn baseline_and_selected_identity_remain_strict() { scenario("baseline-drift"); }

#[test]
fn dirty_final_set_cannot_capture_pending_review() { scenario("pending"); }

#[test]
fn native_failure_timeout_and_eof_precedence_survive() { scenario("precedence"); }

#[test]
fn pre_mutation_publish_rejects_extra_candidates() { scenario("pre-publish"); }

#[test]
fn pre_link_guard_cleans_its_owned_staging_file() { scenario("pre-link"); }

#[test]
fn post_link_failure_preserves_and_reports_history() { scenario("post-link"); }

#[test]
fn unique_native_publication_remains_exact() { scenario("native-success"); }

#[test]
fn public_consult_refuses_unfinished_second_session() { scenario("public-boundary"); }

const PYTHON: &str = r####"import ast, contextlib, hashlib, io, json, os, pathlib, shutil, subprocess, sys, tempfile, time, types
P=pathlib.Path
root=P(sys.argv[1]); case=sys.argv[2]; orch=sys.argv[3]
script=root/'orch/scripts/wake-dsh-stream.sh'
body=script.read_text().split("MSG=\"$msg\" python3 - <<'PY'\n",1)[1].rsplit('\nPY',1)[0]
tree=ast.parse(body)
helper=next(ast.literal_eval(n.value) for n in tree.body if isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='NATIVE_CONSULT_HELPER' for t in n.targets))
(root/'.cowork-temp').mkdir(exist_ok=True)
base=P(tempfile.mkdtemp(prefix='B347-contract-',dir=root/'.cowork-temp'))
pins={'provider':'fixture-provider','model':'fixture-model','reasoningEffort':'max'}
def write(path,data):
 path=P(path);path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(data if isinstance(data,bytes) else data.encode())
def rows(sid,cwd,end=True,pending=None):
 r=[{'type':'session','id':sid,'cwd':str(cwd),'version':1,'createdAt':0},{'type':'turn/start','data':{'turn':1}},{'type':'request/header','data':{'header':{'config':pins}}}]
 if pending:r.append({'type':'tool/call','data':{'callId':'pending-1','name':'write','arguments':{'path':str(pending),'content':'pending review'}}})
 r.append({'type':'assistant/message','data':{'turn':1,'message':{'role':'assistant','content':[{'type':'text','text':'fixture answer'}]}}})
 if end:r.append({'type':'turn/end','data':{'turn':1,'reason':{'kind':'completed'}}})
 return r
def compressed(r):return subprocess.check_output(['zstd','-q','-c'],input=('\n'.join(json.dumps(x) for x in r)+'\n').encode())
def store_session(store,sid,cwd,end=True,pending=None):
 d=P(store)/sid;d.mkdir(parents=True,exist_ok=True);write(d/'session.jsonl.zstd',compressed(rows(sid,cwd,end,pending)));return d
def scanner(label,end=True,pending=False):
 work=base/label;work.mkdir();store=work/'store';store.mkdir();spool=work/'.cowork-temp/review-spool/out.md';spool.parent.mkdir(parents=True)
 ns={'os':os,'time':time,'json':json,'subprocess':subprocess,'stat':__import__('stat'),'shutil':shutil,'sys':sys,'workdir':str(work),'session_root':str(store),'session_store':str(store.parent),'baseline':set(),'selected_dir':None,'selected_id':None,'seen_records':0,'record_frames':0,'last_provider_frame':0,'terminal_seen':False,'terminal_record':None,'pin_verified':False,'receipt_emitted':False,'buffered_projection':[],'buffered_projection_bytes':0,'pending_writes':[],'projected':0,'envelope_mode':pending,'consult_mode':False,'pin_provider':pins['provider'],'pin_model':pins['model'],'pin_effort':pins['reasoningEffort'],'MAX_FRAME_BYTES':65536,'MAX_BUFFERED_PROJECTION_BYTES':4194304,'zstd_bin':shutil.which('zstd'),'seen_candidate_dirs':set(),'selection_error':None,'timed_out':False,'final_set_error':None}
 defs=[n for n in tree.body if isinstance(n,ast.FunctionDef)];exec(compile(ast.Module(body=defs,type_ignores=[]),str(script),'exec'),ns)
 for name in ['scan_selected_session','new_candidates','valid_identity','decode_records','project_record','capture_last_complete_pending_write']:assert callable(ns.get(name)),name
 os.environ['ORCH_HARNESS_REVIEW_OUTPUT_PATH']=str(spool)
 a=store_session(store,'session-A',work,end,spool if pending else None)
 with contextlib.redirect_stdout(io.StringIO()):ns['scan_selected_session']()
 assert ns['selected_id']=='session-A'
 return ns,work,store,a,spool
def conflict(call):
 try:call()
 except RuntimeError:return
 raise AssertionError('second valid/changed identity was accepted')
def final_dirty(ns):
 fn=ns.get('final_candidate_set_error');assert callable(fn),'real final_candidate_set_error helper missing'
 try:return fn() is not None
 except RuntimeError:return True
BACKEND=r'''
import fs from 'node:fs';import path from 'node:path';import cp from 'node:child_process';
export class JsonlSessionPersistence {
 constructor(ctx,config){this.root=config.root;}
 trace(op){
  const temp=fs.existsSync(cfg.nativeRoot)&&fs.readdirSync(cfg.nativeRoot,{recursive:true}).some(x=>String(x).includes('.orch-publish-'));
  const linked=fs.existsSync(cfg.destination);
  if((cfg.stage==='private-read'&&this.root===cfg.privateRoot&&op==='readRaw')||(cfg.stage==='pre-link'&&this.root===cfg.privateRoot&&op==='list'&&temp)||(cfg.stage==='post-link'&&this.root===cfg.nativeRoot&&op==='list'&&linked))inject();
 }
 locate(meta){return {kind:'jsonl',path:path.join(this.root,'--'+meta.cwd.replace(/^\//,'').replace(/\//g,'-')+'--',meta.id,'session.jsonl.zstd')};}
 files(){if(!fs.existsSync(this.root))return [];return fs.readdirSync(this.root,{withFileTypes:true}).filter(p=>p.isDirectory()).flatMap(p=>fs.readdirSync(path.join(this.root,p.name),{withFileTypes:true}).filter(d=>d.isDirectory()).map(d=>path.join(this.root,p.name,d.name,'session.jsonl.zstd')).filter(f=>fs.existsSync(f)));}
 async list(){this.trace('list');const ids=new Set();return this.files().map(f=>{const row=JSON.parse(cp.execFileSync('zstd',['-d','-q','-c',f],{encoding:'utf8'}).split('\n')[0]);if(ids.has(row.id))throw Error('duplicate ID');ids.add(row.id);return row;});}
 async loadStored(id){this.trace('loadStored');const files=this.files().filter(f=>path.basename(path.dirname(f))===id);if(files.length>1)throw Error('duplicate ID');if(!files.length)return undefined;const f=files[0],rows=cp.execFileSync('zstd',['-d','-q','-c',f],{encoding:'utf8'}).trimEnd().split('\n').map(JSON.parse);return {meta:rows[0],events:rows.slice(1),...(fs.existsSync(f+'.torn')?{tornMarker:{}}:{})};}
 async readRaw(id){const loaded=await this.loadStored(id);if(!loaded)return undefined;this.trace('readRaw');return {meta:loaded.meta,content:cp.execFileSync('zstd',['-d','-q','-c',this.locate(loaded.meta).path],{encoding:'utf8'})};}
}
'''
NATIVE=r'''
const fs=require('node:fs'),path=require('node:path'),cp=require('node:child_process');
if(process.argv.includes('--dump-config')){console.log(JSON.stringify([{id:'settings',config:{path:path.join(process.env.DSH_HOME,'settings.yaml')}},{id:'session-persistence-jsonl',config:{root:path.join(process.env.DSH_HOME,'sessions'),compression:'zstd'}}]));process.exit(0);}
const overlay=JSON.parse(fs.readFileSync(process.argv[process.argv.indexOf('--patch')+1]));const conf=id=>overlay.find(x=>x.id===id)?.config;
const settings=JSON.parse(fs.readFileSync(conf('settings').path));const store=conf('session-persistence-jsonl').root,slug='--'+process.cwd().replace(/^\//,'').replace(/\//g,'-')+'--';const id='session-A',d=path.join(store,slug,id);fs.mkdirSync(d,{recursive:true});
const r=[{type:'session',id,cwd:process.cwd(),version:1,createdAt:0},{type:'turn/start',data:{turn:1}},{type:'request/header',data:{header:{config:settings['agent-default-model']}}},{type:'assistant/message',data:{turn:1,message:{role:'assistant',content:[{type:'text',text:'fixture answer'}]}}},{type:'turn/end',data:{turn:1,reason:{kind:'completed'}}}];fs.writeFileSync(path.join(d,'session.jsonl.zstd'),cp.execFileSync('zstd',['-q','-c'],{input:r.map(x=>JSON.stringify(x)).join('\n')+'\n'}));if(publicExtra)fs.mkdirSync(path.join(store,slug,'session-B'));fs.writeFileSync(called,JSON.stringify({privateRoot:store}));process.exit(0);
'''
def fixture(label,stage='none',kind='empty',public=False,extra=False):
 b=base/label;b.mkdir();work=b/'project';work.mkdir();home=b/'home';home.mkdir();private=b/'private';native=home/'sessions';binfile=b/'native/dsh.cjs';called=b/'called.json';slug='--'+str(work).lstrip('/').replace('/','-')+'--';target=native/slug/'session-A/session.jsonl.zstd';othercwd=str(b/'other') if kind.startswith('foreign') else str(work);otherslug='--'+othercwd.lstrip('/').replace('/','-')+'--';other=private/otherslug/'session-B';a=private/slug/'session-A';a.mkdir(parents=True);raw=compressed(rows('session-A',work));write(a/'session.jsonl.zstd',raw)
 cfg={'stage':stage,'privateRoot':str(private),'nativeRoot':str(native),'destination':str(target),'other':str(other),'kind':kind,'extraBytes':list(compressed(rows('session-B',othercwd))),'injected':str(b/'injected')}
 inject=r'''const fs0=await import('node:fs');const path0=await import('node:path');function inject(){if(fs0.existsSync(cfg.injected))return;fs0.mkdirSync(cfg.other,{recursive:true});if(cfg.kind.endsWith('valid'))fs0.writeFileSync(path0.join(cfg.other,'session.jsonl.zstd'),Buffer.from(cfg.extraBytes));if(cfg.kind.endsWith('temp'))fs0.writeFileSync(path0.join(cfg.other,'session.jsonl.zstd.fixture.tmp'),'pending');fs0.writeFileSync(cfg.injected,'yes');}'''
 mods={'@deepseek-ai/cordis':'export class Context {constructor(){this.fiber={dispose:async()=>{}};}}','@deepseek-ai/dsh-session':'export class SessionStore {constructor(ctx){ctx.sessions={list:()=>[]};}}','@deepseek-ai/dsh-session-persistence-jsonl':'const cfg='+json.dumps(cfg)+';\n'+inject+'\n'+BACKEND,'@deepseek-ai/dsh-home-paths':"import path from 'node:path';export const dshHomePath=(...xs)=>path.join(process.env.DSH_HOME,...xs);",'@deepseek-ai/dsh-settings-file':'export const fixture=true;','yaml':"export function parseDocument(text){try{const v=JSON.parse(text);return {errors:[],warnings:[],toJS:()=>v};}catch(e){return {errors:[e]};}}"}
 for name,text in mods.items():
  folder=binfile.parent/'node_modules'/name;write(folder/'package.json',json.dumps({'type':'module','main':'index.js'}));write(folder/'index.js',text)
 write(binfile,'#!/usr/bin/env node\nconst publicExtra='+json.dumps(extra)+',called='+json.dumps(str(called))+';\n'+NATIVE);binfile.chmod(0o755);settings=json.dumps({'agent-default-model':pins});write(home/'settings.yaml',settings)
 return {'base':b,'work':work,'home':home,'private':private,'native':native,'bin':binfile,'target':target,'source':a/'session.jsonl.zstd','bytes':raw,'called':called,'settings':settings,'injected':b/'injected'}
def publish(f):
 program=f['base']/'helper.mjs';write(program,helper);info=f['base']/'info.json';write(info,json.dumps({'bin':str(f['bin']),'home':str(f['home']),'cwd':str(f['work']),'privateRoot':str(f['private']),'nativeRoot':str(f['native']),'id':'session-A','pins':pins}));return subprocess.run(['node',str(program),'publish',str(info)],env=dict(os.environ,DSH_HOME=str(f['home'])),capture_output=True,text=True,timeout=30)
def public_run(f):
 work=f['work'];local_bin=work/'orch/target/debug/orch';local_bin.parent.mkdir(parents=True);shutil.copy2(orch,local_bin);write(work/'orch/target/CACHEDIR.TAG','Signature: 8a477f597d28d172789f06886806bc55\n');write(work/'.gitignore','.orch/\n.cowork-temp/\ncoordination/\norch/target/\n');write(work/'tracked','fixture\n');write(work/'question.md','Read only; return fixture answer.\n');write(work/'.orch/harnesses.yaml',json.dumps({'version':1,'harnesses':{'selected':{'driver':'dsh','enabled':True,'executable':str(f['bin']),'cwdPolicy':'project-root','defaults':{'provider':pins['provider'],'model':pins['model'],'effort':'max'}}}}))
 for args in [['init','-q','-b','main'],['add','.'],['-c','user.name=fixture','-c','user.email=fixture@example.invalid','-c','commit.gpgSign=false','commit','-qm','fixture']]:subprocess.run(['git','-C',str(work)]+args,check=True,capture_output=True)
 result=subprocess.run([str(local_bin),'--root',str(work),'consult','--harness','selected','--member-timeout-secs','30','--total-wall-secs','40',str(work/'question.md')],env=dict(os.environ,DSH_HOME=str(f['home'])),capture_output=True,text=True,timeout=60)
 metas=list((work/'coordination/consultations').glob('*/meta.json'));assert len(metas)==1,(result.stdout,result.stderr);return result,json.loads(metas[0].read_text())
retain_fixture=False
try:
 if case=='late-valid':
  ns,w,store,a,sp=scanner(case);store_session(store,'session-B',w);conflict(ns['scan_selected_session'])
 elif case=='incomplete-lifecycle':
  ns,w,store,a,sp=scanner(case);b=store/'session-B';b.mkdir();ns['scan_selected_session']();assert ns['selected_id']=='session-A';assert final_dirty(ns);store_session(store,'session-B',w);conflict(ns['scan_selected_session'])
 elif case=='remember-disappeared':
  ns,w,store,a,sp=scanner(case);b=store/'session-B';b.mkdir();ns['scan_selected_session']();b.rmdir();assert final_dirty(ns)
 elif case=='stable-final':
  ns,w,store,a,sp=scanner(case);assert not final_dirty(ns);original=ns['new_candidates'];calls=[0]
  def changing():
   calls[0]+=1
   if calls[0]==2:store_session(store,'session-B',w)
   return original()
  ns['new_candidates']=changing;assert final_dirty(ns);assert calls[0]>=2
 elif case=='baseline-drift':
  ns,w,store,a,sp=scanner(case);old=store_session(store,'session-old',w);ns['baseline'].add(str(old));ns['scan_selected_session']();assert ns['selected_id']=='session-A';write(a/'session.jsonl.zstd',compressed([{'type':'session','id':'wrong','cwd':str(w)}]));conflict(ns['scan_selected_session'])
 elif case=='pending':
  ns,w,store,a,sp=scanner('pending-dirty',False,True);(store/'session-B').mkdir();assert not ns['capture_last_complete_pending_write']();assert not sp.exists()
  ns,w,store,a,sp=scanner('pending-clean',False,True);assert ns['capture_last_complete_pending_write']();assert sp.read_bytes()==b'pending review\n'
 elif case=='precedence':
  start=next(i for i,n in enumerate(tree.body) if isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='provider_rc' for t in n.targets));tail=compile(ast.Module(body=tree.body[start:],type_ignores=[]),str(script),'exec')
  for i,(rc,timeout,end,expected) in enumerate([(0,False,True,74),(17,False,False,17),(-9,True,False,72),(0,False,False,71)]):
   ns,w,store,a,sp=scanner('precedence-'+str(i),end,True);(store/'session-B').mkdir();ns.update(process=types.SimpleNamespace(returncode=rc),timed_out=timeout,selection_error=None);stdout=io.StringIO();stderr=io.StringIO();code=None
   with contextlib.redirect_stdout(stdout),contextlib.redirect_stderr(stderr):
    try:exec(tail,ns)
    except SystemExit as e:code=e.code
   assert code==expected,(code,expected,stderr.getvalue());assert not sp.exists();assert 'dsh.terminal' not in stdout.getvalue()
 elif case=='pre-publish':
  for kind in ['valid','empty','temp','foreign-valid','foreign-temp']:
   f=fixture(case+'-'+kind,'private-read',kind);r=publish(f);assert f['injected'].exists();assert r.returncode!=0,r.stdout;assert not f['target'].exists();assert f['source'].read_bytes()==f['bytes']
 elif case=='pre-link':
  f=fixture(case,'pre-link','empty');r=publish(f);assert f['injected'].exists();assert r.returncode!=0;assert not f['target'].exists();assert not list(f['native'].rglob('.orch-publish-*'))
 elif case=='post-link':
  f=fixture(case,'post-link','empty');old=store_session(f['native']/('--'+str(f['work']).lstrip('/').replace('/','-')+'--'),'session-old',f['work']);oldbytes=(old/'session.jsonl.zstd').read_bytes();r=publish(f);assert f['injected'].exists();assert r.returncode!=0,r.stdout;assert f['target'].read_bytes()==f['bytes'];assert (old/'session.jsonl.zstd').read_bytes()==oldbytes;assert not r.stdout.strip();assert 'linked=yes' in r.stderr and 'post-link' in r.stderr and 'session-A' in r.stderr and str(f['target']) in r.stderr,r.stderr
 elif case=='native-success':
  f=fixture(case);r=publish(f);assert r.returncode==0,r.stderr;value=json.loads(r.stdout);assert value['finalText']=='fixture answer';assert f['target'].read_bytes()==f['bytes'];assert value['nativeHistorySha256']==hashlib.sha256(f['bytes']).hexdigest()
 elif case=='public-boundary':
  for extra in [False,True]:
   f=fixture(case+str(extra),public=True,extra=extra);r,meta=public_run(f);member=meta['members'][0];assert (f['home']/'settings.yaml').read_text()==f['settings']
   if extra:assert member['status']!='ok',meta;assert not list(f['native'].rglob('session.jsonl.zstd')) if f['native'].exists() else True;called=json.loads(f['called'].read_text());assert list(P(called['privateRoot']).rglob('session.jsonl.zstd'))
   else:assert r.returncode==0 and member['status']=='ok',(r.stderr,meta);assert list(f['native'].rglob('session.jsonl.zstd'))
 else:raise AssertionError('unknown scenario '+case)
except subprocess.TimeoutExpired:
 retain_fixture=True
 raise
finally:
 if not retain_fixture:shutil.rmtree(base)
"####;
