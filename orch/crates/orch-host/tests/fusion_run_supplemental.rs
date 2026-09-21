//! Production finite coordinator with real owned fake-client subprocesses, no models.
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

#[test]
fn opencode_role_wave_starts_in_stable_order_at_least_one_second_apart() {
    let mut f = Fixture::new();
    let executable = f.root.join("bin/fixture-client");
    fs::write(
        &executable,
        r#"#!/bin/sh
now=$(/usr/bin/python3 -c 'import time;print(time.time_ns())')
case "$*" in *fixture:alpha*) kind=alpha;; *fixture:beta*) kind=beta;; *) kind=other;; esac
printf '%s|%s\n' "$now" "$kind" >> "$(dirname "$0")/../opencode-starts"
exit 1
"#,
    )
    .unwrap();
    fs::write(
        f.root.join(".orch/harnesses.yaml"),
        format!("version: 1\nharnesses:\n  fixture:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: local, model: native-model, effort: high}}\n    consult: {{mode: pure}}\n    cwdPolicy: project-root\n",executable.display()),
    ).unwrap();
    for role in &mut f.config.roles {
        role.fixed = InvocationTuple {
            provider: Some("local".into()),
            model: Some("native-model".into()),
            effort: Some("high".into()),
            mode: None,
        };
    }
    f.config = save_config(&f.root, f.config.revision, &f.config).unwrap();
    fs::create_dir_all(f.context.home.join(".local/share/opencode")).unwrap();
    fs::create_dir_all(f.context.home.join(".local/state/opencode")).unwrap();
    let engine = f.engine();
    engine.start(&f.root, f.request("opencode-stagger")).unwrap();
    let view = f.wait(&engine, "opencode-stagger");
    assert!(matches!(view.phase.as_str(), "failed" | "completed"), "{view:?}");
    let lines = fs::read_to_string(f.root.join("opencode-starts")).unwrap();
    let starts = lines.lines().map(|line| {
        let (stamp, argv) = line.split_once('|').unwrap();
        (stamp.parse::<u128>().unwrap(), argv)
    }).collect::<Vec<_>>();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(starts[0].1, "alpha", "{starts:?}");
    assert_eq!(starts[1].1, "beta", "{starts:?}");
    let phase = f.root.join(".orch/fusion-runs/opencode-stagger/members");
    let first = fs::metadata(phase.join("0.spawned.json")).unwrap().modified().unwrap();
    let second = fs::metadata(phase.join("1.spawned.json")).unwrap().modified().unwrap();
    assert!(second.duration_since(first).unwrap() >= Duration::from_millis(900), "{starts:?}");
}
#[test]
fn real_parallel_roles_forward_native_config_and_freeze_input_before_single_synthesis() {
    let f = Fixture::new();
    let engine = f.engine();
    let req = f.request("run-one");
    engine.start(&f.root, req.clone()).unwrap();
    f.wait_started("alpha");
    f.wait_started("beta");
    assert_eq!(f.calls().len(), 2);
    assert_eq!(engine.start(&f.root, req.clone()).unwrap().id, "run-one");
    let mut conflict = req.clone();
    conflict.question = "different".into();
    assert!(engine.start(&f.root, conflict).is_err());
    let mut edited = f.config.clone();
    edited.roles[0].instructions = "fixture:fail".into();
    save_config(&f.root, 1, &edited).unwrap();
    fs::write(
        Path::new(&f.context.overrides["CLAUDE_CONFIG_DIR"]).join("settings.json"),
        r#"{"model":"later-native","effortLevel":"high"}"#,
    )
    .unwrap();
    f.release();
    let v = f.wait(&engine, "run-one");
    assert_eq!(v.phase, "completed", "{v:?}");
    assert_eq!(v.config_revision, 1);
    assert!(v.members.iter().all(|m| m.status == "verified"));
    assert_eq!(v.synthesis.answer.as_deref(), Some("synthesis answer"));
    let calls = f.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.iter().filter(|c| c["kind"] == "synthesis").count(), 1);
    assert_eq!(
        calls.iter().find(|c| c["kind"] == "alpha").unwrap()["model"],
        "native-model"
    );
    assert_eq!(
        calls.iter().find(|c| c["kind"] == "beta").unwrap()["effort"],
        "future-effort"
    );
    assert_eq!(calls.last().unwrap()["model"], "native-model");
    assert_ne!(
        v.members[0].channel_facts.as_ref().unwrap()["requestDigest"],
        v.members[1].channel_facts.as_ref().unwrap()["requestDigest"]
    );
    engine.start(&f.root, req).unwrap();
    assert_eq!(f.calls().len(), 3);
    fs::write(
        f.root
            .join(".orch/fusion-runs/run-one/members/fusion/0-fusion-0.md"),
        "omega answer",
    )
    .unwrap();
    let changed = engine.read(&f.root, "run-one").unwrap();
    assert!(changed.members[0].answer.is_none());
    assert_eq!(changed.members[0].answer_status.as_deref(), Some("invalid"));
    fs::write(
        f.root
            .join(".orch/fusion-runs/run-one/harness-snapshot.json"),
        "{}",
    )
    .unwrap();
    assert!(engine.read(&f.root, "run-one").is_err());
}
#[test]
fn partial_answers_survive_and_an_independent_reader_does_not_restart_live_work() {
    let f = Fixture::new();
    let mut c = f.config.clone();
    c.roles[0].instructions = "fixture:fail".into();
    save_config(&f.root, 1, &c).unwrap();
    let engine = f.engine();
    engine.start(&f.root, f.request("partial")).unwrap();
    f.wait_started("fail");
    f.wait_started("beta");
    let another = f.engine();
    assert_eq!(another.read(&f.root, "partial").unwrap().phase, "consulting");
    assert!(another.start(&f.root, f.request("do-not-restart")).is_err());
    assert_eq!(f.calls().len(), 2);
    f.release();
    let v = f.wait(&engine, "partial");
    assert_eq!(v.phase, "completed", "{v:?}");
    assert_eq!(v.members[0].status, "failed");
    assert!(v.members[0].answer.is_none());
    assert_eq!(v.members[1].answer.as_deref(), Some("beta answer"));
    assert_eq!(f.calls().len(), 3);
    assert_eq!(another.read(&f.root, "partial").unwrap().phase, "completed");
}

#[test]
fn zero_valid_members_skip_synthesis_and_failed_synthesis_retains_verified_members() {
    for all_fail in [true, false] {
        let f = Fixture::new();
        let mut c = f.config.clone();
        if all_fail {
            c.roles[0].instructions = "fixture:fail".into();
            c.roles[1].instructions = "fixture:fail".into();
            save_config(&f.root, 1, &c).unwrap();
        } else {
            fs::write(f.root.join("fail-synthesis"), "yes").unwrap();
        }
        f.release();
        let e = f.engine();
        e.start(&f.root, f.request("failure")).unwrap();
        let v = f.wait(&e, "failure");
        assert_eq!(v.phase, "failed", "{v:?}");
        assert_eq!(f.calls().len(), if all_fail { 2 } else { 3 });
        assert!(v.synthesis.answer.is_none());
        if !all_fail {
            assert!(v
                .members
                .iter()
                .all(|m| m.answer_status.as_deref() == Some("verified")));
        }
    }
}
#[test]
fn stale_revision_secret_and_symlink_refuse_before_any_client_starts() {
    let f = Fixture::new();
    let e = f.engine();
    assert!(e
        .start_with_revision(&f.root, f.request("stale"), 0)
        .is_err());
    assert!(!f.root.join(".orch/fusion-runs").exists());
    let mut q = f.request("secret");
    q.question = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789".into();
    assert!(e.start(&f.root, q).is_err());
    let target = f.root.join("outside");
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, f.root.join(".orch/fusion-runs")).unwrap();
    assert!(e.start(&f.root, f.request("link")).is_err());
    assert_eq!(f.calls().len(), 0);
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn advancing_project_head_during_members_never_launches_synthesis_on_new_head() {
    let f = Fixture::new();
    let e = f.engine();
    e.start(&f.root, f.request("head-drift")).unwrap();
    f.wait_started("alpha");
    f.wait_started("beta");
    git(
        &f.root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "changed",
        ],
    );
    f.release();
    let v = f.wait(&e, "head-drift");
    assert_eq!(v.phase, "hold");
    assert_eq!(f.calls().len(), 2);
    assert!(v.members.iter().all(|m| m.answer.is_some()));
}

#[test]
fn concurrent_duplicate_requests_never_duplicate_native_work() {
    let f = Fixture::new();
    let e = std::sync::Arc::new(f.engine());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let handles = (0..4)
        .map(|_| {
            let e = e.clone();
            let b = barrier.clone();
            let root = f.root.clone();
            let q = f.request("concurrent");
            std::thread::spawn(move || {
                b.wait();
                e.start(&root, q)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert!(results.iter().any(Result::is_ok));
    f.wait_started("alpha");
    f.wait_started("beta");
    assert_eq!(f.calls().len(), 2);
    f.release();
    assert_eq!(f.wait(&e, "concurrent").phase, "completed");
    assert_eq!(f.calls().len(), 3);
    assert_eq!(
        e.start(&f.root, f.request("concurrent")).unwrap().id,
        "concurrent"
    );
}


fn explicit(f: &Fixture, id: &str) -> orch_host::fusion_run::ConsultRequest {
    orch_host::fusion_run::ConsultRequest { request_id:id.into(),question:"Assess the evidence.".into(),members:vec![f.config.roles[0].clone()],attachments:vec![] }
}
#[test]
fn separate_process_metadata_reader() {
    let Some(root)=std::env::var_os("ORCH_TEST_B364_READER_ROOT") else {return};
    assert_eq!(FusionEngine::new().read_status(Path::new(&root),"cross-process").unwrap().phase,"consulting");
    let entries=FusionEngine::new().list(Path::new(&root)).unwrap();
    assert_eq!(entries.iter().find(|entry|entry.id=="cross-process").unwrap().phase,"consulting");
}
#[test]
fn another_process_observes_owner_without_owning_jobs() {
    let f=Fixture::new();let e=f.engine();e.start_consult(&f.root,explicit(&f,"cross-process")).unwrap();f.wait_started("alpha");
    let output=Command::new(std::env::current_exe().unwrap()).args(["--exact","separate_process_metadata_reader","--nocapture"]).env("ORCH_TEST_B364_READER_ROOT",&f.root).output().unwrap();
    f.release();assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));assert_eq!(f.wait(&e,"cross-process").phase,"completed");
}
#[test]
fn ownerless_unfinished_run_is_hold_without_replay() {
    let f=Fixture::new();f.release();let e=f.engine();let q=explicit(&f,"lost-owner");e.start_consult(&f.root,q.clone()).unwrap();f.wait(&e,"lost-owner");drop(e);
    let state=f.root.join(".orch/fusion-runs/lost-owner/state.json");let mut v:Value=serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();v["phase"]="consulting".into();fs::write(state,serde_json::to_vec_pretty(&v).unwrap()).unwrap();
    let reader=f.engine();assert_eq!(reader.read_status(&f.root,"lost-owner").unwrap().phase,"hold");assert_eq!(reader.list(&f.root).unwrap().iter().find(|entry|entry.id=="lost-owner").unwrap().phase,"hold");assert_eq!(reader.start_consult(&f.root,q).unwrap().phase,"hold");assert_eq!(f.calls().len(),1);
}
#[test]
fn pages_reject_changed_answers_and_invalid_offsets_without_state_prefetch() {
    let f=Fixture::new();f.release();let e=f.engine();e.start_consult(&f.root,explicit(&f,"tamper")).unwrap();f.wait(&e,"tamper");
    assert!(e.read_answer_page(&f.root,"tamper","alpha",1,10,None).is_err());
    assert!(e.read_answer_page(&f.root,"tamper","alpha",999,10,Some("bad")).is_err());
    assert!(e.read_answer_page(&f.root,"tamper","alpha",0,0,None).is_err());
    fs::write(f.root.join(".orch/fusion-runs/tamper/members/fusion/0-fusion-0.md"),"changed").unwrap();
    assert_eq!(e.read_status(&f.root,"tamper").unwrap().members[0].answer,None);
    assert!(e.read_answer_page(&f.root,"tamper","alpha",0,100,None).is_err());
}
#[test]
fn attachments_reject_symlinks_secrets_and_escaping_paths_before_launch() {
    use std::os::unix::fs::symlink;
    let f=Fixture::new();let e=f.engine();let target=f.root.join("facts.txt");fs::write(&target,"facts").unwrap();symlink(&target,f.root.join("link.txt")).unwrap();fs::write(f.root.join(".env"),"not sent").unwrap();
    for path in [f.root.join("link.txt"),f.root.join(".env"),f.root.join("../escape"),PathBuf::from("relative")] {
        let mut q=explicit(&f,"unsafe");q.attachments.push(path);assert!(e.start_consult(&f.root,q).is_err());
    }
    assert!(f.calls().is_empty());
}

#[test]
fn attachment_secret_and_oversize_serialization_refuse_before_reservation() {
    let f=Fixture::new();let e=f.engine();let path=f.root.join("text.txt");
    for content in ["ANTHROPIC_API_KEY=sk-secret-example-key-12345678901234567890".to_string(), "\u{1}".repeat(400_000)] {
        fs::write(&path,content).unwrap();let mut q=explicit(&f,"no-reservation");q.attachments.push(path.clone());
        assert!(e.start_consult(&f.root,q).is_err());
        assert!(!f.root.join(".orch/fusion-runs/no-reservation/request.json").exists());
    }
    assert!(f.calls().is_empty());
}
#[test]
fn pages_preserve_utf8_and_reject_mid_character_offsets() {
    let f=Fixture::new();let script=f.root.join("bin/fixture-client");let s=fs::read_to_string(&script).unwrap().replace("else kind+' answer'", "else '你好世界'");fs::write(script,s).unwrap();f.release();let e=f.engine();e.start_consult(&f.root,explicit(&f,"unicode")).unwrap();f.wait(&e,"unicode");
    let a=e.read_answer_page(&f.root,"unicode","alpha",0,4,None).unwrap();assert_eq!(a.text,"你");assert_eq!(a.next_offset,Some(3));
    assert!(e.read_answer_page(&f.root,"unicode","alpha",1,4,Some(&a.sha256)).is_err());
    assert!(e.read_answer_page(&f.root,"unicode","alpha",0,1,None).is_err());
    let b=e.read_answer_page(&f.root,"unicode","alpha",3,64,Some(&a.sha256)).unwrap();assert_eq!(b.text,"好世界");
}

#[test]
fn matching_replay_waits_for_reservation_transaction_publication() {
    let f=Fixture::new();f.release();let e=f.engine();let q=explicit(&f,"transaction");e.start_consult(&f.root,q.clone()).unwrap();f.wait(&e,"transaction");
    let base=f.root.join(".orch/fusion-runs");let lock=fs::OpenOptions::new().read(true).write(true).open(base.join("run.lock")).unwrap();lock.lock().unwrap();
    let state=base.join("transaction/state.json");let saved=base.join("transaction/state.saved");fs::rename(&state,&saved).unwrap();
    let another=f.engine();let root=f.root.clone();let (tx,rx)=std::sync::mpsc::channel();let reader=std::thread::spawn(move|| {tx.send(()).unwrap();another.start_consult(&root,q)});rx.recv().unwrap();std::thread::sleep(Duration::from_millis(100));
    fs::rename(saved,state).unwrap();drop(lock);
    assert_eq!(reader.join().unwrap().unwrap().phase,"completed");assert_eq!(f.calls().len(),1);
}
#[test]
fn unsupported_explicit_member_fails_before_worker_launch() {
    let f=Fixture::new();let e=f.engine();let mut q=explicit(&f,"missing-harness");q.members[0].harness="configured:missing".into();
    let v=e.start_consult(&f.root,q).unwrap();assert_eq!(v.phase,"failed");assert!(f.calls().is_empty());
}

#[test]
fn explicit_attachments_honor_project_binding_before_reservation() {
    let f=Fixture::new();f.release();let e=f.engine();
    fs::create_dir_all(f.root.join("coordination")).unwrap();
    fs::write(f.root.join("coordination/PROJECT-BINDING.yaml"), r#"{"data":{"forbiddenArtifactPatterns":["private-notes.txt"]}}"#).unwrap();
    let path=f.root.join("private-notes.txt");fs::write(&path,"project-private facts").unwrap();let mut q=explicit(&f,"binding-policy");q.attachments.push(path);
    let error=e.start_consult(&f.root,q).unwrap_err();assert!(error.to_string().contains("命中 PROJECT-BINDING"),"{error:#}");assert!(!f.root.join(".orch/fusion-runs/binding-policy/request.json").exists());assert!(f.calls().is_empty());
}
#[test]
fn explicit_attachments_share_cli_duplicate_rejection() {
    let f=Fixture::new();f.release();let e=f.engine();let path=f.root.join("notes.txt");fs::write(&path,"facts").unwrap();let mut q=explicit(&f,"duplicate-file");q.attachments=vec![path.clone(),path];
    let error=e.start_consult(&f.root,q).unwrap_err();assert!(error.to_string().contains("重复"),"{error:#}");assert!(f.calls().is_empty());
}

#[test]
fn explicit_configured_member_pins_take_precedence_over_native_defaults() {
    let f=Fixture::new();f.release();let e=f.engine();e.start_consult(&f.root,explicit(&f,"configured-pins")).unwrap();
    assert_eq!(f.wait(&e,"configured-pins").phase,"completed");
    let calls=f.calls();assert_eq!(calls.len(),1);
    assert_eq!(calls[0]["model"],"registry-model");assert_eq!(calls[0]["effort"],"high");
}
