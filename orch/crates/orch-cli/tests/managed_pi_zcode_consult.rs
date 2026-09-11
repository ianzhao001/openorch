//! B341: real public CLI -> code-owned wrapper -> model-free native fixture -> archived answer.
//! Every case first requires the new action, so early unsupported-action refusal cannot pass a negative.
//! Mutations: disable either action, drop final body/hash, accept missing native end/empty/nonzero,
//! ignore ZCode pin mismatch, derive cwd from caller, or accept two native terminal objects.
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
    for driver in [HarnessId::Pi, HarnessId::ZCode] {
        assert!(
            driver.supports_action(DriverAction::Consult),
            "{} needs a reachable Consult action",
            driver.as_str()
        );
        assert!(driver.driver_contract(DriverAction::Consult).is_some());
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    parent: PathBuf,
    project: PathBuf,
    other: PathBuf,
    native_home: PathBuf,
    called: PathBuf,
    head: String,
    driver: String,
}
impl Fixture {
    fn new(driver: &str, mode: &str, cwd_policy: &str) -> Self {
        let parent = orch_host::util::test_scratch_dir("b341-managed-consult");
        let project = parent.join("project");
        let other = parent.join("other-project");
        // This is a semantic home for an isolated model-free child account, never a parent HOME rewrite.
        let native_home = parent.join("native-account");
        let called = parent.join("native-call.json");
        fs::create_dir_all(project.join(".orch")).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(native_home.join(".zcode/cli/rollout")).unwrap();
        let native_config = json!({"model":"fixture-provider/fixture-model","provider":{"fixture-provider":{"models":{"fixture-model":{"reasoning":{"levels":["max"],"defaultLevel":"max","providerOptionsByLevel":{"max":{}}}}}}}});
        fs::write(
            native_home.join(".zcode/cli/config.json"),
            serde_json::to_vec(&native_config).unwrap(),
        )
        .unwrap();
        let executable = parent.join(format!("native-{driver}"));
        let setup = format!(
            "driver={}\nmode={}\nanswer={}\ncalled={}\n",
            json!(driver),
            json!(mode),
            json!(ANSWER),
            json!(called.display().to_string())
        );
        let program = r#"#!/usr/bin/python3
import json,os,sys
"#
        .to_owned()
            + &setup
            + r#"
with open(called,'w') as f: json.dump({'cwd':os.getcwd(),'argv':sys.argv[1:]},f)
if driver=='pi':
    print(json.dumps({'type':'session','id':'pi-fixture-session'}))
    print(json.dumps({'type':'turn_start'}))
    print(json.dumps({'type':'tool_execution_end','result':{'content':[{'type':'text','text':'tool output is not the answer'}]}}))
    print(json.dumps({'type':'message_end','message':{'role':'assistant','stopReason':'stop','provider':'fixture-provider','model':'fixture-model','content':[{'type':'text','text': '' if mode=='empty' else answer}]}}))
    if mode!='eof': print(json.dumps({'type':'agent_settled'}))
else:
    value={'sessionId':'zcode-fixture-session','response': '' if mode=='empty' else answer,'usage':{'totalTokens':1}}
    if mode=='duplicate':
        print(json.dumps(value)); value['sessionId']='foreign-session'; print(json.dumps(value))
    else: print(json.dumps(value,indent=2))
sys.exit(19 if mode=='error' else 0)
"#;
        fs::write(&executable, program).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let config = json!({"version":1,"harnesses":{"selected":{"driver":driver,"enabled":true,"executable":executable,"cwdPolicy":cwd_policy,"defaults":{"provider":"fixture-provider","model":"fixture-model","effort":"max"}}}});
        fs::write(
            project.join(".orch/harnesses.yaml"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        fs::write(
            project.join("question.md"),
            "Read-only fixture question; return the native fixture answer.\n",
        )
        .unwrap();
        fs::write(project.join(".gitignore"), ".orch/\ncoordination/\n").unwrap();
        for dir in [&project, &other] {
            git(dir, &["init", "-q", "-b", "main"]);
            fs::write(dir.join("tracked"), "stable\n").unwrap();
            git(dir, &["add", "."]);
            git(dir, &["commit", "-qm", "initial"]);
        }
        let head = git(&project, &["rev-parse", "HEAD"]);
        Self {
            parent,
            project,
            other,
            native_home,
            called,
            head,
            driver: driver.into(),
        }
    }
    fn run(&self) -> (Output, Value, PathBuf) {
        let output = Command::new(env!("CARGO_BIN_EXE_orch"))
            .arg("--root")
            .arg(&self.project)
            .args([
                "consult",
                "--harness",
                "selected",
                "--member-timeout-secs",
                "20",
                "--total-wall-secs",
                "30",
            ])
            .arg("question.md")
            .current_dir(&self.other)
            .env("HOME", &self.native_home)
            .env("RUST_TEST_THREADS", "4")
            .output()
            .unwrap();
        let log = fs::read_to_string(self.project.join("coordination/consultations/log.jsonl"))
            .unwrap_or_else(|_| {
                panic!(
                    "missing consultation record: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let last: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        let dir = self.project.join(last["dir"].as_str().unwrap());
        let meta: Value =
            serde_json::from_slice(&fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(git(&self.project, &["rev-parse", "HEAD"]), self.head);
        (output, meta, dir)
    }
    fn success(&self) {
        let (output, meta, dir) = self.run();
        assert!(
            output.status.success(),
            "{}\n{meta}",
            String::from_utf8_lossy(&output.stderr)
        );
        let m = &meta["members"][0];
        assert_eq!(m["status"], "ok");
        assert_eq!(m["answerExtraction"], "structured");
        assert_eq!(
            fs::read_to_string(dir.join("fusion/0-selected.md")).unwrap(),
            ANSWER
        );
        let facts = &m["channelFacts"];
        assert_eq!(facts["action"], "consult");
        assert_eq!(facts["cwd"], self.project.display().to_string());
        assert_eq!(facts["fixedHead"], self.head);
        assert_eq!(facts["requestedTuple"]["model"], "fixture-model");
        assert_eq!(facts["terminal"]["status"], "answered");
        assert_eq!(facts["terminal"]["finalTextSha256"], ANSWER_SHA);
        assert_eq!(facts["receipt"]["source"], "native");
        assert_eq!(facts["receipt"]["status"], "unknown");
        let call: Value = serde_json::from_slice(&fs::read(&self.called).unwrap()).unwrap();
        assert_eq!(call["cwd"], self.project.display().to_string());
        if self.driver == "zcode" {
            assert!(call["argv"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x == "--cwd"));
        }
    }
    fn failure(&self) {
        let (output, meta, _) = self.run();
        assert!(
            !output.status.success(),
            "invalid native run became a valid answer: {meta}"
        );
        assert_eq!(meta["members"][0]["status"], "failed");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}

#[test]
fn pi_native_final_body_is_a_bound_consult_answer() {
    ready();
    Fixture::new("pi", "ok", "project-root").success();
}
#[test]
fn zcode_native_final_body_is_a_bound_consult_answer() {
    ready();
    Fixture::new("zcode", "ok", "project-root").success();
}
#[test]
fn pi_final_text_without_native_settled_is_not_a_vote() {
    ready();
    Fixture::new("pi", "eof", "project-root").failure();
}
#[test]
fn pi_empty_and_nonzero_native_runs_cannot_pass() {
    ready();
    for mode in ["empty", "error"] {
        Fixture::new("pi", mode, "project-root").failure();
    }
}
#[test]
fn zcode_empty_and_nonzero_native_runs_cannot_pass() {
    ready();
    for mode in ["empty", "error"] {
        Fixture::new("zcode", mode, "project-root").failure();
    }
}
#[test]
fn zcode_pin_mismatch_is_rejected_before_provider_start() {
    ready();
    let f = Fixture::new("zcode", "ok", "project-root");
    let config_path = f.native_home.join(".zcode/cli/config.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["model"] = "fixture-provider/foreign-model".into();
    fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    f.failure();
    assert!(!f.called.exists());
}
#[test]
fn consultation_binds_project_even_from_another_repo_and_target_policy() {
    ready();
    for driver in ["pi", "zcode"] {
        Fixture::new(driver, "ok", "target-worktree").success();
    }
}
#[test]
fn zcode_two_native_sessions_cannot_be_collapsed_into_one_answer() {
    ready();
    Fixture::new("zcode", "duplicate", "project-root").failure();
}

#[test]
fn public_capability_docs_explain_managed_consult_evidence() {
    ready();
    let product = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let source = fs::read_to_string(product.join("orch/crates/orch-host/src/harness.rs")).unwrap();
    let before = source.split("pub fn supports_action(").next().unwrap();
    let docs = before
        .lines()
        .rev()
        .take_while(|line| line.trim().starts_with("///") || line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        docs.contains("Pi")
            && docs.contains("ZCode")
            && docs.contains("Consult")
            && docs.contains("terminal"),
        "public capability docs must describe new managed Consult evidence, not just say supported"
    );
}
