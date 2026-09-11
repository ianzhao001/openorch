//! B342: public consultation, native settings pins and exact native-history publication.
//! Model-free fake DSH package; real product CLI, wrapper, Node and zstd boundaries.
//! Every case requires new Consult support before testing negative behavior.
//! Negative mutations (restore and rerun the full seed after each):
//! M1 disable DSH Consult -> native_final_is_bound_to_original_project.
//! M2 drop native terminal body -> native_final_is_bound_to_original_project.
//! M3 omit snapshot model override -> settings_snapshot_pins_model_and_read_only_without_global_writes.
//! M4 omit native history publication -> native_history_contains_exact_private_bytes_and_final.
//! M5 accept nonzero provider -> incomplete_empty_error_and_failed_reason_are_not_votes.
//! M6 ignore native tornMarker -> native_torn_tail_cannot_be_published_as_complete.
//! M7 ignore pending native calls -> pending_or_mutating_tools_cannot_be_a_valid_consult.
//! M8 remove substantive DSH public docs -> docs_and_old_health_fixtures_use_explicit_native_and_unique_contracts.
use orch_host::harness::{DriverAction, HarnessId};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const ANSWER: &str = "完整咨询答复\nsecond line";
const ANSWER_SHA: &str = "e7fe1964dc2db1aa3384e98870f2e7bc59b16fbe5b4b4d5a65981e1ee374545d";
fn ready() {
    assert!(
        HarnessId::Dsh.supports_action(DriverAction::Consult),
        "DSH Consult must be reachable"
    );
    assert!(HarnessId::Dsh
        .driver_contract(DriverAction::Consult)
        .is_some());
}
fn git(root: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "0")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
struct Fixture {
    parent: PathBuf,
    project: PathBuf,
    other: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    called: PathBuf,
    head: String,
    settings: Vec<u8>,
}
impl Fixture {
    fn new(mode: &str, policy: &str) -> Self {
        let parent = orch_host::util::test_scratch_dir("b342-dsh-consult");
        let project = parent.join("project");
        let other = parent.join("other-project");
        let home = parent.join("native-account");
        let called = parent.join("called.json");
        let bin = project.join("orch/target/debug/orch");
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_orch"), &bin).unwrap();
        write(
            &project.join("orch/target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        let settings=serde_json::to_vec(&json!({"agent-default-model":{"provider":"ambient","model":"wrong-model","reasoningEffort":"low"},"permission":{"defaultPreset":"danger-full-access"},"unrelated":{"preserve":true}})).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("settings.yaml"), &settings).unwrap();
        let native = parent.join("native/dsh.cjs");
        let setup = format!(
            "const mode={}, answer={}, called={};\n",
            json!(mode),
            json!(ANSWER),
            json!(called.display().to_string())
        );
        write(
            &native,
            &("#!/usr/bin/env node\n".to_owned() + &setup + NATIVE),
        );
        fs::set_permissions(&native, fs::Permissions::from_mode(0o755)).unwrap();
        for (name, body) in [("@deepseek-ai/cordis", "export class Context { constructor(){this.fiber={dispose:async()=>{}};} }"),
            ("@deepseek-ai/dsh-session", "export class SessionStore { constructor(ctx){ctx.sessions={list:()=>[]};} }"),
            ("@deepseek-ai/dsh-home-paths", "import path from 'node:path'; export const dshHomePath=(...xs)=>path.join(process.env.DSH_HOME,...xs);"),
            ("@deepseek-ai/dsh-settings-file", "export const fixture=true;"),
            ("yaml", "export function parseDocument(text){try{const value=JSON.parse(text);return {errors:[],warnings:[],toJS:()=>value};}catch(e){return {errors:[e]};}}"),
            ("@deepseek-ai/dsh-session-persistence-jsonl", BACKEND)] {
            let dir=parent.join("native/node_modules").join(name);
            write(&dir.join("package.json"), &json!({"type":"module","main":"index.js"}).to_string());
            write(&dir.join("index.js"),body);
        }
        let config = json!({"version":1,"harnesses":{"selected":{"driver":"dsh","enabled":true,"executable":native,"cwdPolicy":policy,"defaults":{"provider":"fixture-provider","model":"fixture-model","effort":"max"}}}});
        write(&project.join(".orch/harnesses.yaml"), &config.to_string());
        write(
            &project.join("question.md"),
            "Read the project; return the native final response.\n",
        );
        write(
            &project.join(".gitignore"),
            ".orch/\norch/target/\n.cowork-temp/\ncoordination/\n",
        );
        for dir in [&project, &other] {
            fs::create_dir_all(dir).unwrap();
            git(dir, &["init", "-q", "-b", "main"]);
            write(&dir.join("tracked"), "stable\n");
            git(dir, &["add", "."]);
            git(dir, &["commit", "-qm", "initial"]);
        }
        let head = git(&project, &["rev-parse", "HEAD"]);
        Self {
            parent,
            project,
            other,
            home,
            bin,
            called,
            head,
            settings,
        }
    }
    fn run(&self) -> (Output, Value, PathBuf) {
        let o = Command::new(&self.bin)
            .arg("--root")
            .arg(&self.project)
            .args([
                "consult",
                "--harness",
                "selected",
                "--member-timeout-secs",
                "30",
                "--total-wall-secs",
                "40",
                "question.md",
            ])
            .current_dir(&self.other)
            .env("DSH_HOME", &self.home)
            .env("RUST_TEST_THREADS", "4")
            .output()
            .unwrap();
        let text = fs::read_to_string(self.project.join("coordination/consultations/log.jsonl"))
            .unwrap_or_else(|_| {
                panic!(
                    "consult archive absent: {}",
                    String::from_utf8_lossy(&o.stderr)
                )
            });
        let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        let dir = self.project.join(last["dir"].as_str().unwrap());
        let meta = serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(git(&self.project, &["rev-parse", "HEAD"]), self.head);
        assert_eq!(
            fs::read(self.home.join("settings.yaml")).unwrap(),
            self.settings,
            "global settings must stay byte-identical"
        );
        (o, meta, dir)
    }
    fn success(&self) -> Value {
        let (o, m, dir) = self.run();
        assert!(
            o.status.success(),
            "{}\n{m}",
            String::from_utf8_lossy(&o.stderr)
        );
        let member = &m["members"][0];
        assert_eq!(member["status"], "ok");
        assert_eq!(
            fs::read_to_string(dir.join("fusion/0-selected.md")).unwrap(),
            ANSWER
        );
        let facts = &member["channelFacts"];
        assert_eq!(facts["cwd"], self.project.display().to_string());
        assert_eq!(facts["fixedHead"], self.head);
        assert_eq!(facts["terminal"]["status"], "answered");
        assert_eq!(facts["terminal"]["finalTextSha256"], ANSWER_SHA);
        assert_eq!(facts["receipt"]["status"], "unknown");
        serde_json::from_slice(&fs::read(&self.called).unwrap()).unwrap()
    }
    fn failure(&self) {
        let (o, m, _) = self.run();
        assert!(
            !o.status.success(),
            "invalid native session became an answer: {m}"
        );
        assert_eq!(m["members"][0]["status"], "failed");
        let store = self.home.join("sessions");
        if store.exists() {
            for project in fs::read_dir(store).unwrap() {
                for session in fs::read_dir(project.unwrap().path()).unwrap() {
                    let log = session.unwrap().path().join("session.jsonl.zstd");
                    if log.exists() {
                        assert_eq!(
                            fs::read(log).unwrap(),
                            b"existing-native-bytes",
                            "failed call published a new native log"
                        );
                    }
                }
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}

#[test]
fn native_final_is_bound_to_original_project() {
    ready();
    let f = Fixture::new("ok", "project-root");
    let c = f.success();
    assert_eq!(c["cwd"], f.project.display().to_string());
}
#[test]
fn settings_snapshot_pins_model_and_read_only_without_global_writes() {
    ready();
    let f = Fixture::new("ok", "project-root");
    let c = f.success();
    assert_eq!(
        c["settings"]["agent-default-model"],
        json!({"provider":"fixture-provider","model":"fixture-model","reasoningEffort":"max"})
    );
    assert_eq!(c["settings"]["permission"]["defaultPreset"], "read-only");
    assert_eq!(c["settings"]["unrelated"]["preserve"], true);
    assert_eq!(c["settingsWatch"], false);
    assert_eq!(c["settingsMode"].as_u64().unwrap() & 0o777, 0o600);
}
#[test]
fn native_history_contains_exact_private_bytes_and_final() {
    ready();
    let f = Fixture::new("ok", "project-root");
    let c = f.success();
    let p = PathBuf::from(c["privateLog"].as_str().unwrap());
    let projects = fs::read_dir(f.home.join("sessions"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(projects.len(), 1);
    let published = projects[0]
        .path()
        .join("session-b342-fixture/session.jsonl.zstd");
    assert_eq!(fs::read(published).unwrap(), fs::read(p).unwrap());
    let trace = fs::read_to_string(f.home.join("backend-trace.jsonl")).unwrap();
    assert!(trace
        .lines()
        .any(|l| l.contains("\"op\":\"list\"") && l.contains("/sessions\"")));
    assert!(trace
        .lines()
        .any(|l| l.contains("\"op\":\"loadStored\"") && l.contains("/sessions\"")));
}
#[test]
fn incomplete_empty_error_and_failed_reason_are_not_votes() {
    ready();
    for m in ["eof", "empty", "error", "reason", "oversize"] {
        Fixture::new(m, "project-root").failure();
    }
}
#[test]
fn request_pin_drift_or_absence_is_not_a_vote() {
    ready();
    for m in ["pin-drift", "no-pin"] {
        Fixture::new(m, "project-root").failure();
    }
}
#[test]
fn multiple_private_sessions_are_not_selected_by_recency() {
    ready();
    Fixture::new("multiple", "project-root").failure();
}
#[test]
fn pending_or_mutating_tools_cannot_be_a_valid_consult() {
    ready();
    for m in ["pending", "mutating"] {
        Fixture::new(m, "project-root").failure();
    }
}
#[test]
fn native_torn_tail_cannot_be_published_as_complete() {
    ready();
    Fixture::new("torn", "project-root").failure();
}
#[test]
fn existing_native_id_is_never_overwritten_in_any_project() {
    ready();
    for m in ["collision", "foreign-collision"] {
        let f = Fixture::new(m, "project-root");
        f.failure();
        let c: Value = serde_json::from_slice(&fs::read(&f.called).unwrap()).unwrap();
        assert_eq!(
            fs::read(c["collisionPath"].as_str().unwrap()).unwrap(),
            b"existing-native-bytes"
        );
    }
}
#[test]
fn explicit_root_wins_over_target_policy_and_caller_directory() {
    ready();
    let f = Fixture::new("ok", "target-worktree");
    let c = f.success();
    assert_eq!(c["cwd"], f.project.display().to_string());
}
#[test]
fn consult_exception_keeps_runtime_tag_validation() {
    ready();
    let f = Fixture::new("ok", "project-root");
    write(&f.project.join("orch/target/CACHEDIR.TAG"), "wrong\n");
    f.failure();
    assert!(!f.called.exists());
}
#[test]
fn docs_and_old_health_fixtures_use_explicit_native_and_unique_contracts() {
    ready();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let source = fs::read_to_string(root.join("orch/crates/orch-host/src/harness.rs")).unwrap();
    let before = source.split("pub fn supports_action(").next().unwrap();
    let docs = before
        .lines()
        .rev()
        .take_while(|l| l.trim().starts_with("///") || l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(docs.contains("DSH") && docs.contains("native") && docs.contains("history"));
    let health =
        fs::read_to_string(root.join("orch/crates/orch-host/tests/dsh_session_health.rs")).unwrap();
    assert!(
        health.contains("test_scratch_dir("),
        "health fixtures must use the existing owned unique scratch helper"
    );
    assert!(
        !health.contains("SystemTime::now()"),
        "wall-clock timestamps are not unique fixture identities"
    );
}

const NATIVE: &str = r#"
const fs=require('node:fs'), path=require('node:path'), cp=require('node:child_process');
if(process.argv.includes('--dump-config')){console.log(JSON.stringify([{id:'settings',config:{path:path.join(process.env.DSH_HOME,'settings.yaml')}},{id:'session-persistence-jsonl',config:{root:path.join(process.env.DSH_HOME,'sessions'),compression:'zstd'}}]));process.exit(0);}
const overlay=JSON.parse(fs.readFileSync(process.argv[process.argv.indexOf('--patch')+1]));
const byId=id=>overlay.find(x=>x.id===id)?.config;
const settingConfig=byId('settings');const settings=settingConfig?JSON.parse(fs.readFileSync(settingConfig.path)):JSON.parse(fs.readFileSync(path.join(process.env.DSH_HOME,'settings.yaml')));
const store=byId('session-persistence-jsonl').root;const id='session-b342-fixture';
const slug='--'+process.cwd().replace(/^\//,'').replace(/\//g,'-')+'--';const dir=path.join(store,slug,id);fs.mkdirSync(dir,{recursive:true});
const file=path.join(dir,'session.jsonl.zstd');
const pins=settings['agent-default-model'];if(mode==='pin-drift')pins.model='drifted-model';
const records=[{type:'session',id,cwd:process.cwd(),version:1,createdAt:0},{type:'turn/start',data:{turn:1}}];
if(mode!=='no-pin')records.push({type:'request/header',data:{header:{config:pins}}});
if(mode==='pending'||mode==='mutating')records.push({type:'tool/call',data:{callId:'call-1',name:mode==='mutating'?'write':'read',arguments:{}}});
if(mode==='mutating')records.push({type:'tool/result',data:{message:{content:[{type:'tool-result',toolCallId:'call-1',content:[],isError:false}]}}});
records.push({type:'assistant/message',data:{turn:1,message:{role:'assistant',content:[{type:'text',text:mode==='empty'?'':mode==='oversize'?answer.repeat(10000):answer}]}}});
if(mode!=='eof')records.push({type:'turn/end',data:{turn:1,reason:{kind:mode==='reason'?'failed':'completed'}}});
const raw=records.map(x=>JSON.stringify(x)).join('\n')+'\n';fs.writeFileSync(file,cp.execFileSync('zstd',['-q','-c'],{input:raw}));
if(mode==='torn')fs.writeFileSync(file+'.torn','torn marker fixture');
if(mode==='multiple'){const other=path.join(store,slug,'session-second');fs.mkdirSync(other,{recursive:true});records[0].id='session-second';fs.writeFileSync(path.join(other,'session.jsonl.zstd'),cp.execFileSync('zstd',['-q','-c'],{input:records.map(x=>JSON.stringify(x)).join('\n')+'\n'}));}
let collisionPath=null;if(mode==='collision'||mode==='foreign-collision'){collisionPath=path.join(process.env.DSH_HOME,'sessions',mode==='collision'?slug:'--foreign-project--',id,'session.jsonl.zstd');fs.mkdirSync(path.dirname(collisionPath),{recursive:true});fs.writeFileSync(collisionPath,'existing-native-bytes');}
fs.writeFileSync(called,JSON.stringify({cwd:process.cwd(),settings,settingsWatch:settingConfig?.watch,settingsMode:settingConfig?fs.statSync(settingConfig.path).mode:null,privateLog:file,collisionPath}));
process.exit(mode==='error'?19:0);
"#;
const BACKEND: &str = r#"
import fs from 'node:fs';import path from 'node:path';import cp from 'node:child_process';
export class JsonlSessionPersistence {
 constructor(ctx,config){this.root=config.root;}
 trace(op){fs.appendFileSync(path.join(process.env.DSH_HOME,'backend-trace.jsonl'),JSON.stringify({op,root:this.root})+'\n');}
 locate(meta){return {kind:'jsonl',path:path.join(this.root,'--'+meta.cwd.replace(/^\//,'').replace(/\//g,'-')+'--',meta.id,'session.jsonl.zstd')};}
 files(){if(!fs.existsSync(this.root))return [];return fs.readdirSync(this.root).flatMap(p=>fs.readdirSync(path.join(this.root,p)).map(id=>path.join(this.root,p,id,'session.jsonl.zstd')).filter(f=>fs.existsSync(f)));}
 async list(){this.trace('list');const ids=new Set();return this.files().map(f=>{const id=path.basename(path.dirname(f));if(ids.has(id))throw Error('duplicate native ID');ids.add(id);return JSON.parse(cp.execFileSync('zstd',['-d','-q','-c',f],{encoding:'utf8'}).split('\n')[0]);});}
 async loadStored(id){this.trace('loadStored');const files=this.files().filter(f=>path.basename(path.dirname(f))===id);if(files.length>1)throw Error('duplicate native ID');if(!files.length)return undefined;const f=files[0];const rows=cp.execFileSync('zstd',['-d','-q','-c',f],{encoding:'utf8'}).trimEnd().split('\n').map(JSON.parse);return {meta:rows[0],events:rows.slice(1),...(fs.existsSync(f+'.torn')?{tornMarker:{}}:{})};}
 async readRaw(id){const loaded=await this.loadStored(id);if(!loaded)return undefined;return {meta:loaded.meta,content:cp.execFileSync('zstd',['-d','-q','-c',this.locate(loaded.meta).path],{encoding:'utf8'})};}
}
"#;
