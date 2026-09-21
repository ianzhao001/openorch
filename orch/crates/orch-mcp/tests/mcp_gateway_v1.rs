//! B365 frozen MCP gateway contract. Mutate four-tool exposure, allowlist, metadata projection, wait bound and digest forwarding independently; restore exact bytes. Black-box stdio coverage is additionally required.
use orch_host::{
    channel::InvocationTuple,
    fusion_roles::{save_config, FusionCombination, FusionConfig, FusionRole},
    fusion_run::{FusionEngine, FusionRequest, RunView},
    native_discovery::DiscoveryContext,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
fn git(r: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null"
        ])
        .args(args)
        .current_dir(r)
        .status()
        .unwrap()
        .success());
}
struct Fixture {
    root: PathBuf,
    context: DiscoveryContext,
    config: FusionConfig,
}
impl Fixture {
    fn new() -> Self {
        let root = orch_host::util::test_scratch_dir("fusion real 中文");
        git(&root, &["init", "-q"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        );
        let home = root.join("native home");
        let native = home.join("custom config");
        let bin = root.join("bin");
        fs::create_dir_all(&native).unwrap();
        fs::create_dir(&bin).unwrap();
        fs::write(
            native.join("settings.json"),
            r#"{"model":"native-model","effortLevel":"off"}"#,
        )
        .unwrap();
        fs::write(native.join("marker"), "native-config-routed").unwrap();
        let exe = bin.join("fixture-client");
        fs::write(&exe,r#"#!/usr/bin/python3
import sys,os,json,time
from pathlib import Path
args=sys.argv[1:]
assert '--dangerously-skip-permissions' not in args
assert args[args.index('--permission-mode')+1]=='plan'
assert args[args.index('--tools')+1]=='Read,Glob,Grep'
assert (Path(os.environ['CLAUDE_CONFIG_DIR'])/'marker').read_text()=='native-config-routed'
prompt=args[args.index('-p')+1]
kind='synthesis' if 'fixture:synthesis' in prompt else 'fail' if 'fixture:fail' in prompt else 'alpha' if 'fixture:alpha' in prompt else 'beta'
base=Path(__file__).parent.parent
record={'kind':kind,'prompt':prompt,'argv':args,'model':args[args.index('--model')+1],'effort':args[args.index('--effort')+1]}
fd=os.open(str(base/'calls.jsonl'),os.O_WRONLY|os.O_APPEND|os.O_CREAT,0o600)
os.write(fd,(json.dumps(record)+'\n').encode());os.close(fd)
if kind!='synthesis':
 (base/('started-'+kind)).write_text('started')
 deadline=time.monotonic()+4
 while not (base/'release').exists() and time.monotonic()<deadline:time.sleep(.01)
 if not (base/'release').exists():sys.exit(83)
if kind=='synthesis':
 assert 'beta answer' in prompt
 assert 'untrusted failed answer' not in prompt
answer='synthesis answer' if kind=='synthesis' else 'untrusted failed answer' if kind=='fail' else kind+' answer'
print(json.dumps({'type':'system','subtype':'init','model':record['model']}))
print(json.dumps({'type':'result','subtype':'error_during_execution' if kind=='fail' or (kind=='synthesis' and (base/'fail-synthesis').exists()) else 'success','is_error':kind=='fail' or (kind=='synthesis' and (base/'fail-synthesis').exists()),'result':answer}))
"#).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(root.join(".orch")).unwrap();
        fs::write(root.join(".gitignore"), ".orch/\n").unwrap();
        fs::write(root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  fixture:\n    driver: claude\n    executable: {}\n    enabled: true\n    defaults: {{model: registry-model, effort: high}}\n    cwdPolicy: project-root\n",exe.display())).unwrap();
        let roles = [
            ("alpha", "fixture:alpha"),
            ("beta", "fixture:beta"),
            ("synth", "fixture:synthesis"),
        ]
        .into_iter()
        .map(|(id, instructions)| FusionRole {
            id: id.into(),
            name: id.into(),
            instructions: instructions.into(),
            harness: "configured:fixture".into(),
            fixed: if id == "beta" {
                InvocationTuple {
                    model: Some("future/model-next".into()),
                    effort: Some("future-effort".into()),
                    ..Default::default()
                }
            } else {
                InvocationTuple::default()
            },
        })
        .collect();
        let config = save_config(
            &root,
            0,
            &FusionConfig {
                revision: 0,
                roles,
                combinations: vec![FusionCombination {
                    id: "pair".into(),
                    name: "Pair".into(),
                    members: vec!["alpha".into(), "beta".into()],
                    disabled: vec![],
                    synthesizer: Some("synth".into()),
                }],
            },
        )
        .unwrap();
        let context = DiscoveryContext {
            project: root.clone(),
            home,
            search_path: vec![bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
            query_timeout_ms: 2000,
            allow_commands: false,
            overrides: BTreeMap::from([(
                "CLAUDE_CONFIG_DIR".into(),
                native.to_string_lossy().into_owned(),
            )]),
            include_platform_locations: false,
        };
        Self {
            root,
            context,
            config,
        }
    }
    fn engine(&self) -> FusionEngine {
        FusionEngine::with_discovery_context(self.context.clone())
    }
    fn request(&self, id: &str) -> FusionRequest {
        FusionRequest {
            request_id: id.into(),
            combination_id: "pair".into(),
            question: "Assess the evidence.".into(),
        }
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    fn release(&self) {
        fs::write(self.root.join("release"), "go").unwrap();
    }
    fn wait_started(&self, kind: &str) {
        let start = Instant::now();
        while !self.root.join(format!("started-{kind}")).exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "role did not start; calls={:?}",
                self.calls()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn wait(&self, e: &FusionEngine, id: &str) -> RunView {
        let start = Instant::now();
        loop {
            let v = e.read(&self.root, id).unwrap();
            if matches!(v.phase.as_str(), "completed" | "failed" | "hold") {
                return v;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "unfinished {v:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

use orch_mcp::service::Gateway;
use serde_json::json;

fn gateway(f:&Fixture)->Gateway { Gateway::with_engine(vec![f.root.clone()], f.engine()).unwrap() }
fn invoke(g:&Gateway,name:&str,args:Value)->anyhow::Result<Value> {
 tokio::runtime::Runtime::new().unwrap().block_on(g.invoke(name,args))
}
fn payload(f:&Fixture,id:&str)->Value {
 json!({"project":f.root,"requestKey":id,"question":"Assess the evidence.","members":[f.config.roles[0]],"attachments":[]})
}
#[test]
fn four_tools_and_project_boundary() {
 let f=Fixture::new();let g=gateway(&f);
 let mut names=g.tools().iter().map(|t|t.name.to_string()).collect::<Vec<_>>();names.sort();
 assert_eq!(names,vec!["consult","get_run","list_harnesses","read_answer"]);
 assert!(invoke(&g,"list_harnesses",json!({"project":"/"})).is_err());
 assert!(invoke(&g,"consult",json!({"project":f.root,"command":"echo unsafe"})).is_err());
 assert!(f.calls().is_empty());
}
#[test]
fn reservation_replay_metadata_and_verified_pages() {
 let f=Fixture::new();f.release();let g=gateway(&f);let p=payload(&f,"mcp-one");
 let a=invoke(&g,"consult",p.clone()).unwrap();assert_eq!(a["id"],"mcp-one");
 let mut v=invoke(&g,"get_run",json!({"project":f.root,"runId":"mcp-one","waitMs":15000})).unwrap();
 assert_eq!(v["phase"],"completed");assert!(v.get("question").is_none());
 assert!(v["members"][0].get("answer").is_none());assert!(v["members"][0].get("channelFacts").is_none());
 invoke(&g,"consult",p.clone()).unwrap();assert_eq!(f.calls().len(),1);
 v=p;v["question"]=json!("changed");assert!(invoke(&g,"consult",v).is_err());
 let a=invoke(&g,"read_answer",json!({"project":f.root,"runId":"mcp-one","memberId":"alpha","offset":0,"limit":5})).unwrap();
 assert_eq!(a["text"],"alpha");assert_eq!(a["nextOffset"],5);
 let b=invoke(&g,"read_answer",json!({"project":f.root,"runId":"mcp-one","memberId":"alpha","offset":5,"limit":100,"sha256":a["sha256"]})).unwrap();assert_eq!(b["text"]," answer");
 assert!(invoke(&g,"read_answer",json!({"project":f.root,"runId":"mcp-one","memberId":"alpha","offset":5,"limit":100,"sha256":"wrong"})).is_err());
}
#[test]
fn independent_gateway_keeps_live_owner_and_wait_bound() {
 let f=Fixture::new();let g=gateway(&f);invoke(&g,"consult",payload(&f,"live")).unwrap();f.wait_started("alpha");
 let other=gateway(&f);
 assert_eq!(invoke(&other,"get_run",json!({"project":f.root,"runId":"live","waitMs":0})).unwrap()["phase"],"consulting");
 assert!(invoke(&other,"get_run",json!({"project":f.root,"runId":"live","waitMs":30001})).is_err());
 f.release();assert_eq!(invoke(&g,"get_run",json!({"project":f.root,"runId":"live","waitMs":15000})).unwrap()["phase"],"completed");
}
