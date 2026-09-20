//! The real ZCode wrapper reads a fake native client's private settings and receipts.
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
struct Fixture {
    root: PathBuf,
    private: PathBuf,
    config: PathBuf,
    exe: PathBuf,
}
impl Fixture {
    fn new(mode: &str, size: usize) -> Self {
        let root = orch_host::util::test_scratch_dir("fusion-zcode");
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        ] {
            assert!(Command::new("git")
                .args(["-c", "core.fsmonitor=false"])
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success());
        }
        let private = root.join(".orch/fusion-runs/test/members/private/fusion-0");
        fs::create_dir_all(&private).unwrap();
        let config = root.join("native-config.json");
        fs::write(&config,serde_json::to_vec(&json!({"model":{"main":"p/old","title":"p/old"},"provider":{"p":{"kind":"openai","apiKey":"native-credential-canary","models":{"old":{},"chosen":{"reasoning":{"levels":["off","future-effort"],"defaultLevel":"off","providerOptionsByLevel":{"off":{},"future-effort":{"opaqueOption":7}}}}}}},"unrelated":{"keep":true}})).unwrap()).unwrap();
        fs::write(
            root.join("scenario.json"),
            json!({"mode":mode,"size":size}).to_string(),
        )
        .unwrap();
        let exe = root.join("native-client");
        fs::write(&exe,r#"#!/usr/bin/python3
import os,sys,json,datetime,uuid,stat
from pathlib import Path
root=Path(__file__).parent
cfg=json.loads((root/'scenario.json').read_text())
args=sys.argv[1:]
assert args[args.index('--mode')+1]=='plan'
settings=Path(args[args.index('--settings')+1])
p=json.loads(settings.read_text())
assert p['model']['main']=='p/chosen' and p['model']['title']=='p/old'
assert p['provider']['p']['kind']=='openai' and p['provider']['p']['apiKey']=='native-credential-canary'
assert p['provider']['p']['models']['chosen']['reasoning']['defaultLevel']=='future-effort'
assert p['unrelated']['keep'] is True
assert stat.S_IMODE(settings.stat().st_mode)==0o600
assert 'ZCODE_MODEL' not in os.environ and 'ZCODE_BASE_URL' not in os.environ and 'ZCODE_SESSION_DB' not in os.environ
storage=Path(os.environ['ZCODE_STORAGE_DIR']);db=Path(os.environ['ZCODE_SESSION_DB_PATH'])
assert settings.parent==storage.parent and db==storage/'cli/db/db.sqlite'
db.write_text('private session only')
(root/'launched.json').write_text(json.dumps({'settings':str(settings),'storage':str(storage),'db':str(db)}))
if cfg['mode']=='overflow':
 sys.stdout.write(' '*(9*1024*1024))
session='sess_'+uuid.uuid4().hex;trace=str(uuid.uuid4());turn='turn-'+uuid.uuid4().hex
answer='z'*cfg['size']+' verified'
now=datetime.datetime.now(datetime.timezone.utc).isoformat()
row={'type':'model_io','querySource':'main_turn','sessionId':session,'traceId':trace,'turnId':turn,'startedAt':now,'completedAt':now,'model':{'providerId':'p','modelId':'chosen','variant':'future-effort'},'response':{'modelId':'native-api-canonical','text':answer,'finishReason':'stop'},'error':None}
if cfg['mode']=='wrong-model':row['model']['modelId']='other'
if cfg['mode']=='wrong-effort':row['model']['variant']='off'
if cfg['mode']=='wrong-answer':row['response']['text']='not-the-terminal'
rows=[row]
if cfg['mode']=='duplicate':rows.append(row.copy())
# Unrelated native title/subagent work cannot authorize or veto the main answer.
rows.append({**row,'querySource':'subagent','traceId':'other-trace','model':{'providerId':'other','modelId':'other','variant':None}})
rollout=storage/'cli/rollout'/('model-io-'+session+'.jsonl')
rollout.write_text(''.join(json.dumps(r)+'\n' for r in rows))
print(json.dumps({'sessionId':session,'traceId':trace,'response':answer}))
"#).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            root,
            private,
            config,
            exe,
        }
    }
    fn run(&self) -> Output {
        let wrapper = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join("orch/scripts/wake-zcode-stream.sh");
        Command::new("/bin/sh")
            .arg(wrapper)
            .arg("Read-only fixture question")
            .current_dir(&self.root)
            .env("ORCH_ZCODE_CONFIG", &self.config)
            .env(
                "ORCH_ZCODE_ROLLOUT_DIR",
                self.root.join("absent-global-rollout"),
            )
            .env("ORCH_FUSION_PRIVATE_ROOT", &self.private)
            .env("ORCH_FUSION_ROLE", "1")
            .env("ORCH_HARNESS_ID", "zcode")
            .env("ORCH_HARNESS_ACTION_ID", "fixture")
            .env("ORCH_HARNESS_WAKE_ID", "fixture")
            .env("ORCH_HARNESS_ROUND", "manual")
            .env("ORCH_HARNESS_TASK_ID", "CONSULT")
            .env("ORCH_HARNESS_ATTEMPT_ID", "CONSULT-A0000")
            .env("ORCH_HARNESS_ROLE", "consult")
            .env("ORCH_HARNESS_CWD", &self.root)
            .env("ORCH_HARNESS_FIXED_HEAD", "0".repeat(40))
            .env("ORCH_HARNESS_PROVIDER", "p")
            .env("ORCH_HARNESS_MODEL", "chosen")
            .env("ORCH_HARNESS_EFFORT", "future-effort")
            .env("ORCH_HARNESS_PROVIDER_BIN", &self.exe)
            .env("ORCH_HARNESS_REVIEW_OUTPUT_PATH", "none")
            .env("ORCH_HARNESS_ORCH_BIN", "/bin/true")
            .env("ORCH_HARNESS_DEADLINE_SECS", "10")
            .env("ZCODE_MODEL", "must-not-override")
            .env("ZCODE_BASE_URL", "https://must-not-use.invalid")
            .env(
                "ZCODE_SESSION_DB",
                self.root.join("must-not-write-global.sqlite"),
            )
            .output()
            .unwrap()
    }
}
#[test]
fn selected_native_model_uses_private_settings_and_large_terminal_is_drained() {
    let f = Fixture::new("valid", 160 * 1024);
    let before = fs::read(&f.config).unwrap();
    let start = Instant::now();
    let output = f.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(fs::read(&f.config).unwrap(), before);
    assert!(!f.root.join("must-not-write-global.sqlite").exists());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("native-credential-canary"));
    let frame: Value = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "zcode.terminal")
        .unwrap();
    assert_eq!(
        frame["pinEvidence"]["responseModel"],
        "native-api-canonical"
    );
    assert_eq!(frame["pinEvidence"]["effort"], "future-effort");
    assert!(frame["finalText"].as_str().unwrap().len() > 128 * 1024);
}
#[test]
fn native_mismatch_duplicate_and_oversize_output_never_release_an_answer() {
    for mode in [
        "wrong-model",
        "wrong-effort",
        "wrong-answer",
        "duplicate",
        "overflow",
    ] {
        let f = Fixture::new(mode, 12);
        let output = f.run();
        assert!(!output.status.success(), "{mode}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("\"type\":\"zcode.terminal\""),
            "{mode}"
        );
    }
}
#[test]
fn project_model_override_refuses_before_native_start() {
    let f = Fixture::new("valid", 12);
    fs::write(f.root.join("zcode.json"), r#"{"model":"another/model"}"#).unwrap();
    let before = fs::read(&f.config).unwrap();
    assert!(!f.run().status.success());
    assert!(!f.root.join("launched.json").exists());
    assert_eq!(fs::read(&f.config).unwrap(), before);
}

#[test]
fn production_archives_zcode_answers_then_cleans_only_owned_private_settings() {
    use orch_host::{
        channel::InvocationTuple,
        fusion_roles::{save_config, FusionCombination, FusionConfig, FusionRole},
        fusion_run::{FusionEngine, FusionRequest},
        native_discovery::DiscoveryContext,
    };
    let f = Fixture::new("valid", 12);
    fs::write(f.root.join(".gitignore"), ".orch/\n").unwrap();
    fs::write(f.root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  fixture:\n    driver: zcode\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",f.exe.display())).unwrap();
    let roles = ["a", "b", "s"]
        .into_iter()
        .map(|id| FusionRole {
            id: id.into(),
            name: id.into(),
            instructions: "Read evidence".into(),
            harness: "configured:fixture".into(),
            fixed: InvocationTuple {
                provider: Some("p".into()),
                model: Some("chosen".into()),
                effort: Some("future-effort".into()),
                mode: None,
            },
        })
        .collect();
    save_config(
        &f.root,
        0,
        &FusionConfig {
            revision: 0,
            roles,
            combinations: vec![FusionCombination {
                id: "pair".into(),
                name: "Pair".into(),
                members: vec!["a".into(), "b".into()],
                disabled: vec![],
                synthesizer: Some("s".into()),
            }],
        },
    )
    .unwrap();
    let e = FusionEngine::with_discovery_context(DiscoveryContext {
        project: f.root.clone(),
        home: f.root.clone(),
        search_path: vec!["/usr/bin".into(), "/bin".into()],
        query_timeout_ms: 1000,
        allow_commands: false,
        overrides: std::collections::BTreeMap::from([(
            "ORCH_ZCODE_CONFIG".into(),
            f.config.to_string_lossy().into_owned(),
        )]),
        include_platform_locations: false,
    });
    let before = fs::read(&f.config).unwrap();
    e.start(
        &f.root,
        FusionRequest {
            request_id: "owned".into(),
            combination_id: "pair".into(),
            question: "Compare evidence".into(),
        },
    )
    .unwrap();
    let start = Instant::now();
    let result = loop {
        let v = e.read(&f.root, "owned").unwrap();
        if matches!(v.phase.as_str(), "completed" | "failed" | "hold") {
            break v;
        }
        assert!(start.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(result.phase, "completed", "{result:?}");
    assert!(result.members.iter().all(|m| m.answer.is_some()));
    assert!(result.synthesis.answer.is_some());
    assert_eq!(fs::read(&f.config).unwrap(), before);
    for (phase, alias) in [
        ("members", "fusion-0"),
        ("members", "fusion-1"),
        ("synthesis", "fusion-synthesis"),
    ] {
        let private = f
            .root
            .join(format!(".orch/fusion-runs/owned/{phase}/private/{alias}"));
        assert!(!private.join("settings.json").exists());
        assert!(private.join("storage/cli/db/db.sqlite").exists());
    }
}
