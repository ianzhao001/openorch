//! B364 shared consultation contract. Mutate validation, no-synthesis, ownership, idempotency, metadata-only reads and answer pagination independently.
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

use orch_host::fusion_run::ConsultRequest;

fn request(f: &Fixture, id: &str) -> ConsultRequest {
    ConsultRequest { request_id:id.into(), question:"Assess the evidence.".into(), members:vec![f.config.roles[0].clone()], attachments:vec![] }
}
#[test]
fn explicit_request_is_bounded_and_unique() {
    let f=Fixture::new(); let mut q=request(&f,"one");
    assert!(q.validate().is_ok()); q.members.clear(); assert!(q.validate().is_err());
    q=request(&f,"one"); q.members.push(q.members[0].clone()); assert!(q.validate().is_err());
    q=request(&f,"../bad"); assert!(q.validate().is_err());
    q=request(&f,"one"); q.question=" ".into(); assert!(q.validate().is_err());
}
#[test]
fn single_member_never_starts_synthesis_and_replay_is_idempotent() {
    let f=Fixture::new();f.release();let e=f.engine();let q=request(&f,"single");
    e.start_consult(&f.root,q.clone()).unwrap();let v=f.wait(&e,"single");
    assert_eq!(v.phase,"completed");assert_eq!(v.members.len(),1);assert_eq!(v.synthesis.status,"skipped");
    assert_eq!(f.calls().len(),1);e.start_consult(&f.root,q.clone()).unwrap();assert_eq!(f.calls().len(),1);
    let mut changed=q;changed.question.push('!');assert!(e.start_consult(&f.root,changed).is_err());
}
#[test]
fn independent_reader_does_not_claim_live_owner_missing() {
    let f=Fixture::new();let e=f.engine();e.start_consult(&f.root,request(&f,"owner")).unwrap();
    f.wait_started("alpha");let reader=f.engine();
    assert_eq!(reader.read_status(&f.root,"owner").unwrap().phase,"consulting");
    f.release();assert_eq!(f.wait(&e,"owner").phase,"completed");
}
#[test]
fn metadata_has_no_answer_and_pages_are_bound_to_verified_text() {
    let f=Fixture::new();f.release();let e=f.engine();e.start_consult(&f.root,request(&f,"pages")).unwrap();f.wait(&e,"pages");
    assert!(e.read_status(&f.root,"pages").unwrap().members[0].answer.is_none());
    let a=e.read_answer_page(&f.root,"pages","alpha",0,5,None).unwrap();
    assert_eq!(a.text,"alpha");assert_eq!(a.next_offset,Some(5));
    let b=e.read_answer_page(&f.root,"pages","alpha",5,100,Some(&a.sha256)).unwrap();
    assert_eq!(b.text," answer");assert_eq!(b.next_offset,None);
    assert!(e.read_answer_page(&f.root,"pages","alpha",5,100,Some("wrong")).is_err());
    assert!(e.read_answer_page(&f.root,"pages","missing",0,100,None).is_err());
}
#[test]
fn attachment_bytes_are_snapshotted_and_changed_replay_is_rejected() {
    let f=Fixture::new();let e=f.engine();let path=f.root.join("facts.txt");fs::write(&path,"immutable facts").unwrap();
    let mut q=request(&f,"files");q.attachments.push(path.clone());e.start_consult(&f.root,q.clone()).unwrap();
    f.wait_started("alpha");fs::write(&path,"changed facts").unwrap();f.release();f.wait(&e,"files");
    assert!(f.calls()[0]["prompt"].as_str().unwrap().contains("immutable facts"));
    assert!(e.start_consult(&f.root,q).is_err());
}
