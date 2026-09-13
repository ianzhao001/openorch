//! DSH settings snapshot contract: real facade/wrapper, model-free native fixture.
//! Guards: snapshot pins/consumption, non-consult permission preservation, DSH_HOME isolation,
//! watch:false/private0600/unique output, collision refusal, preserved legacy preset patch with pinned snapshot,
//! valid missing settings, malformed/non-map/symlink source, later external global change,
//! and no unused history-destination restriction. Existing Consult/legacy suites remain sentinels.
//! Individual mutations: bypass snapshot for nonconsult; drop DSH_HOME passthrough; omit each
//! requested pin; keep watch enabled; loosen output mode or overwrite collision; accept bad source;
//! re-read global after snapshot; restore over external writer; omit missing-source pinning;
//! replace native consumed pins with env echo; force readonly on nonconsult; validate unused root;
//! remove retained preset patch; remove each substantive public/guide boundary. Restore exactly
//! and rerun the complete seed after each applicable semantic mutation; compile errors do not count.
#![cfg(unix)]
use orch_host::channel::{
    capture_attachment_manifest_v1, preflight_invocation_v1, prepare_invocation,
    render_invocation_v1, run_preflighted_invocation_v1, ChannelExecution, InvocationAction,
    InvocationContextV1, InvocationRequest, RenderedInvocationV1,
};
use orch_host::harness_config::parse_harness_config_snapshot;
use serde_json::{json, Value};
use std::{
    cell::Cell,
    ffi::OsString,
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};
static ENV: Mutex<()> = Mutex::new(());
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);
impl EnvGuard {
    fn set(values: Vec<(&'static str, Option<OsString>)>) -> Self {
        let old = values
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in values {
            if let Some(v) = v {
                std::env::set_var(k, v)
            } else {
                std::env::remove_var(k)
            }
        }
        Self(old)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            if let Some(v) = v {
                std::env::set_var(k, v)
            } else {
                std::env::remove_var(k)
            }
        }
    }
}
fn put(p: &Path, s: impl AsRef<[u8]>) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}
fn executable(p: &Path, s: impl AsRef<[u8]>) {
    put(p, s);
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}
fn git(p: &Path, args: &[&str]) -> String {
    let r = Command::new("git")
        .arg("-C")
        .arg(p)
        .args(args)
        .output()
        .unwrap();
    assert!(
        r.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    String::from_utf8(r.stdout).unwrap().trim().into()
}
fn pins(action: InvocationAction) -> Value {
    if action == InvocationAction::Execute {
        json!({"provider":"requested-two","model":"model-two","reasoningEffort":"max"})
    } else {
        json!({"provider":"requested-one","model":"model-one","reasoningEffort":"high"})
    }
}
struct Fixture {
    case: PathBuf,
    project: PathBuf,
    worktree: PathBuf,
    home: PathBuf,
    default_home: PathBuf,
    bin: PathBuf,
    orch: PathBuf,
    head: String,
    cfg: PathBuf,
    trace: PathBuf,
    retain: Cell<bool>,
}
impl Fixture {
    fn new() -> Self {
        let parent = std::env::current_dir().unwrap().join(".cowork-temp");
        fs::create_dir_all(&parent).unwrap();
        let case = parent.join(format!("B348-snapshot-{}", ulid::Ulid::new()));
        fs::create_dir(&case).unwrap();
        let case = fs::canonicalize(case).unwrap();
        let project = case.join("project");
        fs::create_dir(&project).unwrap();
        git(&project, &["init", "-q"]);
        git(&project, &["config", "core.fsmonitor", "false"]);
        put(&project.join("tracked"), "fixture\n");
        git(&project, &["add", "tracked"]);
        git(
            &project,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let head = git(&project, &["rev-parse", "HEAD"]);
        let worktree = case.join("worktree");
        git(
            &project,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                worktree.to_str().unwrap(),
                &head,
            ],
        );
        let home = case.join("native-home");
        let default_home = case.join("default-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(default_home.join(".dsh")).unwrap();
        let orch = case.join("runtime-target/debug/orch");
        executable(&orch, "#!/bin/sh\nexit 0\n");
        put(
            &case.join("runtime-target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        );
        let bin = case.join("native/dsh.cjs");
        let cfg = case.join("fixture.json");
        let trace = case.join("trace.json");
        put(&cfg, serde_json::to_vec(&json!({"trace":trace})).unwrap());
        put(&trace, b"{\"queries\":0,\"providers\":0}");
        executable(
            &bin,
            FAKE.replace("__CFG__", &serde_json::to_string(&cfg).unwrap()),
        );
        for(name,source)in[("@deepseek-ai/dsh-settings-file","export const fixture=true;"),("yaml","export function parseDocument(text){try{let v=JSON.parse(text);return {errors:[],warnings:[],toJS:()=>v};}catch(e){return {errors:[e],warnings:[]};}}")]{let dir=case.join("native/node_modules").join(name);put(&dir.join("package.json"),br#"{"type":"module","main":"index.js"}"#);put(&dir.join("index.js"),source);}
        let f = Self {
            case,
            project,
            worktree,
            home,
            default_home,
            bin,
            orch,
            head,
            cfg,
            trace,
            retain: Cell::new(false),
        };
        f.settings(json!({"agent-default-model":{"provider":"user","model":"user-default","reasoningEffort":"low"},"unrelated":{"keep":true}}));
        f
    }
    fn env(&self) -> EnvGuard {
        EnvGuard::set(vec![
            ("HOME", Some(self.default_home.clone().into_os_string())),
            ("DSH_HOME", Some(self.home.clone().into_os_string())),
            ("ORCH_DSH_PRESET", None),
            ("ORCH_DSH_PROFILE", None),
        ])
    }
    fn settings(&self, v: Value) {
        for p in [
            self.home.join("settings.yaml"),
            self.default_home.join(".dsh/settings.yaml"),
        ] {
            put(&p, serde_json::to_vec(&v).unwrap())
        }
    }
    fn option(&self, k: &str, v: Value) {
        let mut c: Value = serde_json::from_slice(&fs::read(&self.cfg).unwrap()).unwrap();
        c[k] = v;
        put(&self.cfg, serde_json::to_vec(&c).unwrap());
    }
    fn trace(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.trace).unwrap()).unwrap()
    }
    fn render(&self, action: InvocationAction, mode: Option<&str>) -> RenderedInvocationV1 {
        let p = pins(action);
        let mut defaults =
            json!({"provider":p["provider"],"model":p["model"],"effort":p["reasoningEffort"]});
        if let Some(mode) = mode {
            defaults["mode"] = mode.into();
        }
        let config = json!({"version":1,"harnesses":{"fixture":{"driver":"dsh","executable":self.bin,"enabled":true,"defaults":defaults,"cwdPolicy":"target-worktree"}}});
        let snapshot = parse_harness_config_snapshot(
            &self.project.join(".orch/harnesses.yaml"),
            &config.to_string(),
        )
        .unwrap();
        let prepared = prepare_invocation(
            &snapshot,
            InvocationRequest {
                alias: "fixture".into(),
                action,
                prompt: "Read-only fixture selection check".into(),
                project_root: self.project.clone(),
                target_worktree: self.worktree.clone(),
                target_head: self.head.clone(),
                attachments: capture_attachment_manifest_v1(&[]).unwrap(),
            },
        )
        .unwrap();
        render_invocation_v1(
            prepared,
            InvocationContextV1 {
                action_id: ulid::Ulid::new().to_string(),
                wake_id: ulid::Ulid::new().to_string(),
                round: "r88".into(),
                task_id: "B348".into(),
                attempt_id: "B348-A0001".into(),
                review_output: (action == InvocationAction::Review)
                    .then(|| self.worktree.join("review.md")),
                orch_executable: self.orch.clone(),
                deadline_secs: 30,
            },
        )
        .unwrap()
    }
    fn run(&self, action: InvocationAction, mode: Option<&str>) -> ChannelExecution {
        let ready = preflight_invocation_v1(self.render(action, mode)).unwrap();
        self.retain.set(true);
        let r = run_preflighted_invocation_v1(ready).expect("real wrapper invocation");
        assert!(
            r.process_group_terminated,
            "retain fixture if native scope end is unknown"
        );
        self.retain.set(false);
        r
    }
    fn ok(&self, r: &ChannelExecution) {
        assert!(
            r.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&r.stdout),
            String::from_utf8_lossy(&r.stderr)
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.retain.get() {
            let _ = fs::remove_dir_all(&self.case);
        }
    }
}
fn fixture(test: impl FnOnce(&Fixture)) {
    let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let _env = f.env();
    test(&f)
}
#[test]
fn selected_actions_use_native_snapshot_and_distinct_tuples() {
    fixture(|f| {
        let before = fs::read(f.home.join("settings.yaml")).unwrap();
        let mut paths = Vec::new();
        for a in [InvocationAction::Review, InvocationAction::Execute] {
            let rendered = f.render(a, None);
            assert_eq!(
                rendered.env().get("DSH_HOME"),
                Some(&f.home.to_string_lossy().into_owned())
            );
            f.ok(&f.run(a, None));
            let t = f.trace();
            assert_eq!(t["selected"], pins(a));
            assert_eq!(t["home"], json!(f.home));
            assert_eq!(t["settings"]["unrelated"], json!({"keep":true}));
            paths.push(t["settingsPath"].clone());
        }
        assert_ne!(paths[0], paths[1]);
        assert_eq!(fs::read(f.home.join("settings.yaml")).unwrap(), before);
    });
}
#[test]
fn private_snapshot_is_0600_and_watch_disabled() {
    fixture(|f| {
        f.settings(json!({"agent-default-model":pins(InvocationAction::Review)}));
        f.ok(&f.run(InvocationAction::Review, Some("headless")));
        let t = f.trace();
        assert_eq!(t["watch"], false);
        assert_eq!(t["mode"], 0o600);
        assert_ne!(t["settingsPath"], json!(f.home.join("settings.yaml")));
        assert!(t["settingsPath"]
            .as_str()
            .unwrap()
            .starts_with(f.case.join("runtime-target").to_str().unwrap()));
    });
}
#[test]
fn explicit_preset_keeps_compatibility_while_snapshot_pins_model() {
    fixture(|f| {
        let r = f.run(InvocationAction::Review, Some("minimal"));
        f.ok(&r);
        let t = f.trace();
        assert_eq!(t["queries"], 1);
        assert_eq!(t["providers"], 1);
        assert_eq!(t["selected"], pins(InvocationAction::Review));
        assert!(t["patch"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == "agent-presets" && x["config"]["default"] == "minimal"));
    });
}
#[test]
fn existing_snapshot_output_is_not_overwritten() {
    fixture(|f| {
        f.settings(json!({"agent-default-model":pins(InvocationAction::Review)}));
        f.option("collision", true.into());
        let r = f.run(InvocationAction::Review, None);
        assert!(!r.success());
        let t = f.trace();
        assert_eq!(t["queries"], 1);
        assert_eq!(t["providers"], 0);
        assert_eq!(
            fs::read(t["collisionPath"].as_str().unwrap()).unwrap(),
            b"preserved fixture sentinel"
        );
    });
}
#[test]
fn malformed_source_refuses_before_provider() {
    for kind in ["broken", "array", "symlink"] {
        fixture(|f| {
            f.settings(json!({"agent-default-model":pins(InvocationAction::Review)}));
            for source in [
                f.home.join("settings.yaml"),
                f.default_home.join(".dsh/settings.yaml"),
            ] {
                match kind {
                    "broken" => put(&source, "not valid fixture JSON"),
                    "array" => put(&source, "[]"),
                    _ => {
                        let victim = source.with_extension("victim");
                        fs::rename(&source, &victim).unwrap();
                        symlink(victim, &source).unwrap();
                    }
                }
            }
            let r = f.run(InvocationAction::Review, None);
            assert!(!r.success());
            assert_eq!(f.trace()["providers"], 0);
            assert_eq!(f.trace()["queries"], 1);
        });
    }
}
#[test]
fn later_global_change_does_not_override_or_invalidate_snapshot() {
    fixture(|f| {
        f.option("laterGlobal",json!({"agent-default-model":{"provider":"later","model":"later","reasoningEffort":"low"}}));
        f.ok(&f.run(InvocationAction::Review, None));
        assert_eq!(f.trace()["selected"], pins(InvocationAction::Review));
        let changed: Value =
            serde_json::from_slice(&fs::read(f.home.join("settings.yaml")).unwrap()).unwrap();
        assert_eq!(changed["agent-default-model"]["provider"], "later");
    });
}
#[test]
fn missing_settings_remains_valid_empty_map() {
    fixture(|f| {
        fs::remove_file(f.home.join("settings.yaml")).unwrap();
        fs::remove_file(f.default_home.join(".dsh/settings.yaml")).unwrap();
        f.ok(&f.run(InvocationAction::Review, None));
        assert_eq!(f.trace()["selected"], pins(InvocationAction::Review));
        assert!(!f.home.join("settings.yaml").exists());
    });
}
#[test]
fn consumed_private_snapshot_drift_is_not_hidden_by_env_echo() {
    fixture(|f| {
        f.option("tamperPrivate", true.into());
        let r = f.run(InvocationAction::Review, None);
        assert_eq!(r.exit_code(), Some(74));
        let t = f.trace();
        assert_eq!(t["tampered"], true);
        assert_eq!(t["selected"]["provider"], "tampered");
        assert_eq!(t["providers"], 1);
    });
}
#[test]
fn nonconsult_permission_values_and_absence_are_preserved() {
    for permission in [Some(json!({"defaultPreset":"workspace-write"})), None] {
        fixture(|f| {
            let mut s = json!({"agent-default-model":pins(InvocationAction::Review)});
            if let Some(v) = permission.clone() {
                s["permission"] = v;
            }
            f.settings(s);
            f.ok(&f.run(InvocationAction::Review, None));
            assert_eq!(f.trace()["settings"].get("permission"), permission.as_ref());
            assert!(!f.trace()["patch"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["id"] == "permission" || x.get("disabled") == Some(&json!(true))));
        });
    }
}
#[test]
fn nonconsult_does_not_require_unused_history_destination() {
    fixture(|f| {
        f.settings(json!({"agent-default-model":pins(InvocationAction::Review)}));
        f.option("unusedRootExpression", true.into());
        f.ok(&f.run(InvocationAction::Review, None));
    });
}
#[test]
fn public_docs_describe_snapshot_permission_and_preset_boundaries() {
    let source = include_str!("../src/channel.rs");
    let at = source.find("pub fn render_invocation_v1(").unwrap();
    let docs = source[..at]
        .lines()
        .rev()
        .take_while(|line| line.trim().is_empty() || line.trim().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    for required in ["DSH_HOME", "snapshot", "Consult", "permission"] {
        assert!(
            docs.contains(required),
            "public renderer docs need {required}"
        );
    }
    let guide = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    let marker = "<!-- orch-guide-review:dsh-envelope-settings -->";
    let section = guide
        .split(marker)
        .nth(1)
        .expect("substantive DSH envelope settings guide section");
    let text = section.lines().take(12).collect::<Vec<_>>().join("\n");
    for required in ["DSH_HOME", "watch:false", "Review", "Execute", "preset"] {
        assert!(
            text.contains(required),
            "DSH guide section needs {required}"
        );
    }
}

const FAKE: &str = r###"#!/usr/bin/env node
const fs=require('node:fs'),path=require('node:path'),cp=require('node:child_process');
const cfg=JSON.parse(fs.readFileSync(__CFG__)), home=process.env.DSH_HOME||path.join(process.env.HOME,'.dsh');
const readTrace=()=>JSON.parse(fs.readFileSync(cfg.trace));const save=t=>fs.writeFileSync(cfg.trace,JSON.stringify(t));
if(process.argv.includes('--dump-config')){
 let t=readTrace();t.queries++;if(cfg.collision){const target=path.dirname(path.dirname(process.env.ORCH_HARNESS_ORCH_BIN));const dirs=fs.readdirSync(target).filter(x=>x.startsWith('.orch-dsh-sessions-'));if(dirs.length!==1)throw Error('fixture needs one exact invocation directory');const nonce=dirs[0].slice('.orch-dsh-sessions-'.length);t.collisionPath=path.join(target,'.orch-dsh-settings-'+nonce+'.json');fs.writeFileSync(t.collisionPath,'preserved fixture sentinel');}save(t);
 console.log(JSON.stringify([{id:'settings',config:{path:path.join(home,'settings.yaml')}},{id:'session-persistence-jsonl',config:{root:cfg.unusedRootExpression?{nativeExpression:'unmodeledGlobalRoot()'}:path.join(home,'sessions'),compression:'zstd'}}]));process.exit(0);
}
let t=readTrace();t.providers++;save(t);
const patch=JSON.parse(fs.readFileSync(process.argv[process.argv.indexOf('--patch')+1]));const entry=id=>patch.find(x=>x.id===id)?.config;
const privateRoot=entry('session-persistence-jsonl').root, settingsEntry=entry('settings'), settingsPath=settingsEntry?.path||path.join(home,'settings.yaml');
if(cfg.laterGlobal)fs.writeFileSync(path.join(home,'settings.yaml'),JSON.stringify(cfg.laterGlobal));
if(cfg.tamperPrivate){if(!settingsEntry?.path)process.exit(99);fs.writeFileSync(settingsPath,JSON.stringify({'agent-default-model':{provider:'tampered',model:'tampered',reasoningEffort:'low'}}));t.tampered=true;}
let settings=fs.existsSync(settingsPath)?JSON.parse(fs.readFileSync(settingsPath)):{};
const base=entry('agent-default-model')||{}, selected={provider:base.provider,model:base.model,...settings['agent-default-model']};
Object.assign(t,{home,settingsPath,settings,selected,patch,watch:settingsEntry?.watch??null,mode:fs.existsSync(settingsPath)?fs.statSync(settingsPath).mode&0o777:null});save(t);
const cwd=process.cwd(),sid='session-settings-fixture',slug='--'+cwd.replace(/^\//,'').replace(/\//g,'-')+'--',dir=path.join(privateRoot,slug,sid);fs.mkdirSync(dir,{recursive:true});
const rows=[{type:'session',id:sid,cwd,version:1,createdAt:0},{type:'turn/start',data:{turn:1}},{type:'request/header',data:{header:{config:selected}}},{type:'assistant/message',data:{turn:1,message:{role:'assistant',content:[{type:'text',text:'fixture answer'}]}}},{type:'turn/end',data:{turn:1,reason:{kind:'completed'}}}];
fs.writeFileSync(path.join(dir,'session.jsonl.zstd'),cp.execFileSync('zstd',['-q','-c'],{input:rows.map(x=>JSON.stringify(x)).join('\n')+'\n'}));
"###;
