//! Draft B343 contract: explicit original-project history, independently of tool cwd.
//! M1 omit Pi project root; M2 use tool cwd for session dir; M3 omit --session-dir;
//! M4 re-disable CodeBuddy persistence; M5 accept malformed project root;
//! M6 silently continue separated-root Pi without its native SDK; M7 remove public history docs.
use orch_host::channel::{
    capture_attachment_manifest_v1, prepare_invocation, render_invocation_v1, InvocationAction,
    InvocationContextV1, InvocationRequest, RenderedInvocationV1,
};
use orch_host::harness_config::parse_harness_config_snapshot;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    work: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    called: PathBuf,
    sdk_called: PathBuf,
    history: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = orch_host::util::test_scratch_dir("b343-project-history");
        let project = root.join("original-project");
        let work = root.join("tool-worktree");
        let home = root.join("native-account");
        for p in [&project, &work, &home] {
            fs::create_dir_all(p).unwrap();
        }
        let bin = root.join("native-pi/dist/bundle/cli.js");
        let called = root.join("provider-called.json");
        let sdk_called = root.join("sdk-called.json");
        let history = root.join("native-original-project-history");
        write(
            &root.join("native-pi/package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent","type":"module","version":"0.85.1"}"#,
        );
        let sdk=format!("import fs from 'node:fs'; export function getDefaultSessionDir(project){{fs.writeFileSync({},JSON.stringify({{project}}));fs.mkdirSync({},{{recursive:true}});return {};}}\n",json!(sdk_called),json!(history),json!(history));
        write(&root.join("native-pi/dist/core/session-manager.js"), &sdk);
        let provider = format!(
            r#"#!/usr/bin/python3
import json,os,sys
args=sys.argv[1:]
directory=args[args.index('--session-dir')+1] if '--session-dir' in args else None
with open({},'w') as f:json.dump({{'cwd':os.getcwd(),'argv':args,'sessionDir':directory}},f)
if directory:
 os.makedirs(directory,exist_ok=True)
 with open(os.path.join(directory,'fixture-session.jsonl'),'w') as f:f.write(json.dumps({{'type':'session','id':'fixture-session','cwd':os.getcwd()}})+'\n'+json.dumps({{'type':'message','message':{{'role':'assistant','content':[{{'type':'text','text':'history fixture'}}]}}}})+'\n')
print(json.dumps({{'type':'session','id':'fixture-session'}}))
print(json.dumps({{'type':'message_end','message':{{'role':'assistant','stopReason':'stop','provider':'fixture','model':'fixture-model','content':[{{'type':'text','text':'history fixture'}}]}}}}))
print(json.dumps({{'type':'agent_settled'}}))
"#,
            json!(called)
        );
        write(&bin, &provider);
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            project,
            work,
            home,
            bin,
            called,
            sdk_called,
            history,
        }
    }
    fn render(&self, driver: &str, action: InvocationAction, same: bool) -> RenderedInvocationV1 {
        let defaults = if driver == "pi" {
            json!({"provider":"fixture","model":"fixture-model","effort":"max"})
        } else {
            json!({"model":"fixture-model","effort":"max"})
        };
        let config=json!({"version":1,"harnesses":{"chosen":{"driver":driver,"executable":self.bin,"enabled":true,"defaults":defaults,"cwdPolicy":"target-worktree"}}}).to_string();
        let snapshot =
            parse_harness_config_snapshot(&self.project.join(".orch/harnesses.yaml"), &config)
                .unwrap();
        let req = InvocationRequest {
            alias: "chosen".into(),
            action,
            prompt: "bounded model-free history fixture".into(),
            project_root: self.project.clone(),
            target_worktree: if same {
                self.project.clone()
            } else {
                self.work.clone()
            },
            target_head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            attachments: capture_attachment_manifest_v1(&[]).unwrap(),
        };
        let prepared = prepare_invocation(&snapshot, req).unwrap();
        render_invocation_v1(
            prepared,
            InvocationContextV1 {
                action_id: "b343-action".into(),
                wake_id: "b343-wake".into(),
                round: "r87".into(),
                task_id: "B343".into(),
                attempt_id: "B343-A0001".into(),
                review_output: if action == InvocationAction::Review {
                    Some(self.work.join("review.md"))
                } else {
                    None
                },
                orch_executable: PathBuf::from("/bin/echo"),
                deadline_secs: 20,
            },
        )
        .unwrap()
    }
    fn run(&self, same: bool, override_root: Option<&str>) -> Output {
        let invocation = self.render("pi", InvocationAction::Execute, same);
        let mut command = Command::new("/bin/sh");
        command
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .ancestors()
                    .nth(3)
                    .unwrap()
                    .join("orch/scripts/wake-pi-stream.sh"),
            )
            .arg("bounded fixture")
            .env_clear()
            .envs(invocation.env())
            .env("HOME", &self.home)
            .current_dir(&self.root);
        if let Some(root) = override_root {
            command.env("ORCH_PI_PROJECT_ROOT", root);
        }
        command.output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn ready() {
    let f = Fixture::new();
    let p = f.render("pi", InvocationAction::Execute, false);
    assert_eq!(
        p.env().get("ORCH_PI_PROJECT_ROOT"),
        Some(&f.project.display().to_string()),
        "Pi must explicitly receive original-project identity"
    );
    let c = f.render("codebuddy", InvocationAction::Consult, true);
    assert!(
        !c.argv().iter().any(|x| x == "--no-session-persistence"),
        "CodeBuddy must preserve native history"
    );
}

#[test]
fn rendered_pi_roles_keep_tool_cwd_and_explicit_project() {
    ready();
    let f = Fixture::new();
    for action in [InvocationAction::Execute, InvocationAction::Review] {
        let r = f.render("pi", action, false);
        assert_eq!(r.prepared().cwd(), f.work);
        assert_eq!(
            r.env().get("ORCH_HARNESS_CWD"),
            Some(&f.work.display().to_string())
        );
        assert_eq!(
            r.env().get("ORCH_PI_PROJECT_ROOT"),
            Some(&f.project.display().to_string())
        );
    }
}
#[test]
fn codebuddy_argv_keeps_native_persistence_enabled() {
    ready();
    let f = Fixture::new();
    let r = f.render("codebuddy", InvocationAction::Consult, true);
    assert!(!r.argv().iter().any(|x| x == "--no-session-persistence"));
    assert_eq!(r.prepared().cwd(), f.project);
}
#[test]
fn pi_wrapper_uses_native_project_session_dir_without_moving_tools() {
    ready();
    let f = Fixture::new();
    let o = f.run(false, None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let call: Value = serde_json::from_slice(&fs::read(&f.called).unwrap()).unwrap();
    let sdk: Value = serde_json::from_slice(&fs::read(&f.sdk_called).unwrap()).unwrap();
    assert_eq!(sdk["project"], f.project.display().to_string());
    assert_eq!(call["cwd"], f.work.display().to_string());
    assert_eq!(call["sessionDir"], f.history.display().to_string());
    let history = fs::read_to_string(f.history.join("fixture-session.jsonl")).unwrap();
    let header: Value = serde_json::from_str(history.lines().next().unwrap()).unwrap();
    assert_eq!(header["cwd"], f.work.display().to_string());
    assert!(history.contains("history fixture"));
}
#[test]
fn malformed_project_root_fails_before_native_start() {
    ready();
    for invalid in ["", "relative/project", "/definitely/missing/b343-project"] {
        let f = Fixture::new();
        let o = f.run(false, Some(invalid));
        assert!(!o.status.success());
        assert!(!f.called.exists());
    }
}
#[test]
fn missing_native_sdk_cannot_fall_back_to_worktree_history() {
    ready();
    let f = Fixture::new();
    fs::remove_file(f.root.join("native-pi/dist/core/session-manager.js")).unwrap();
    let o = f.run(false, None);
    assert!(!o.status.success());
    assert!(!f.called.exists());
}
#[test]
fn same_root_pi_keeps_existing_native_default_path() {
    ready();
    let f = Fixture::new();
    fs::remove_file(f.root.join("native-pi/dist/core/session-manager.js")).unwrap();
    let r = f.render("pi", InvocationAction::Consult, true);
    assert_eq!(
        r.env().get("ORCH_PI_PROJECT_ROOT"),
        Some(&f.project.display().to_string())
    );
    let o = f.run(true, None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let call: Value = serde_json::from_slice(&fs::read(&f.called).unwrap()).unwrap();
    assert_eq!(call["cwd"], f.project.display().to_string());
    assert!(!f.sdk_called.exists());
}

#[test]
fn public_render_docs_explain_native_project_history() {
    ready();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join("orch/crates/orch-host/src/channel.rs");
    let text = fs::read_to_string(source).unwrap();
    let before = text.split("pub fn render_invocation_v1(").next().unwrap();
    let docs = before
        .lines()
        .rev()
        .take_while(|line| line.trim().starts_with("///") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        docs.contains("Pi")
            && docs.contains("project")
            && docs.contains("history")
            && docs.contains("CodeBuddy"),
        "public render contract must explain new native history behavior"
    );
}
