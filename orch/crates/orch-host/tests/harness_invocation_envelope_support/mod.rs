//! Real shell scenes for the frozen B294 invocation-envelope contract.
//!
//! The helpers only create repository-local executables/configuration, launch
//! the production wrappers, and read bytes written by the provider stubs. All
//! envelope admission decisions remain in production Rust or shell code.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

pub const PI_WRAPPER: &str = "orch/scripts/wake-pi-stream.sh";
pub const ZCODE_WRAPPER: &str = "orch/scripts/wake-zcode-stream.sh";
pub const DSH_WRAPPER: &str = "orch/scripts/wake-dsh-stream.sh";
pub const MANAGED_WRAPPERS: [&str; 3] = [PI_WRAPPER, ZCODE_WRAPPER, DSH_WRAPPER];

static NEXT_SCENE: AtomicU64 = AtomicU64::new(0);

pub struct RawOutcome {
    pub exit_code: i32,
    pub stderr: String,
}

pub struct AliasOutcome {
    pub observed_model: Option<String>,
    pub stderr: String,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("orch-host manifest should be nested under the repository root")
        .to_path_buf()
}

fn executable(path: &Path, bytes: &[u8]) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create stub parent");
    }
    fs::write(path, bytes).expect("write provider stub");
    let mut permissions = fs::metadata(path)
        .expect("stat provider stub")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make provider stub executable");
    path.to_path_buf()
}

fn find_executable(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate.is_file()
                && fs::metadata(candidate)
                    .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
        })
        .unwrap_or_else(|| panic!("required test executable is not on PATH: {name}"))
}

pub fn scratch_root(tag: &str) -> PathBuf {
    let sequence = NEXT_SCENE.fetch_add(1, Ordering::Relaxed);
    let root = repo_root()
        .join("orch/target/test-tmp")
        .join(format!("b294-{tag}-{}-{sequence}", std::process::id()));
    for directory in [
        "site",
        "runtime-target",
        "dsh-home",
        "rollout",
        "bin",
        "coordination/runtime",
    ] {
        fs::create_dir_all(root.join(directory)).expect("create B294 scratch directory");
    }
    root
}

pub fn valid_envelope(root: &Path) -> BTreeMap<String, String> {
    let provider = executable(
        &root.join("bin/provider-placeholder"),
        b"#!/bin/sh\nexit 0\n",
    );
    let orch = executable(&root.join("bin/orch-placeholder"), b"#!/bin/sh\nexit 0\n");
    BTreeMap::from([
        ("ORCH_HARNESS_ID".to_string(), "pi".to_string()),
        (
            "ORCH_HARNESS_ACTION_ID".to_string(),
            "action-b294-0001".to_string(),
        ),
        (
            "ORCH_HARNESS_WAKE_ID".to_string(),
            "wake-b294-0001".to_string(),
        ),
        ("ORCH_HARNESS_ROUND".to_string(), "r77".to_string()),
        ("ORCH_HARNESS_TASK_ID".to_string(), "B294".to_string()),
        (
            "ORCH_HARNESS_ATTEMPT_ID".to_string(),
            "B294-A0001".to_string(),
        ),
        ("ORCH_HARNESS_ROLE".to_string(), "secondary".to_string()),
        (
            "ORCH_HARNESS_CWD".to_string(),
            root.join("site").display().to_string(),
        ),
        (
            "ORCH_HARNESS_FIXED_HEAD".to_string(),
            "4d2ca5a800371c67158bdabc0e0fec10a993eee7".to_string(),
        ),
        (
            "ORCH_HARNESS_PROVIDER".to_string(),
            "fixture-provider".to_string(),
        ),
        (
            "ORCH_HARNESS_MODEL".to_string(),
            "fixture-model".to_string(),
        ),
        ("ORCH_HARNESS_EFFORT".to_string(), "high".to_string()),
        (
            "ORCH_HARNESS_PROVIDER_BIN".to_string(),
            provider.display().to_string(),
        ),
        (
            "ORCH_HARNESS_REVIEW_OUTPUT_PATH".to_string(),
            root.join("review.md").display().to_string(),
        ),
        (
            "ORCH_HARNESS_ORCH_BIN".to_string(),
            orch.display().to_string(),
        ),
        ("ORCH_HARNESS_DEADLINE_SECS".to_string(), "30".to_string()),
    ])
}

fn provider_capture_prelude() -> &'static str {
    r#"import json
import os

KEYS = [
    "ORCH_HARNESS_ID", "ORCH_HARNESS_ACTION_ID", "ORCH_HARNESS_WAKE_ID",
    "ORCH_HARNESS_ROUND", "ORCH_HARNESS_TASK_ID", "ORCH_HARNESS_ATTEMPT_ID",
    "ORCH_HARNESS_ROLE", "ORCH_HARNESS_CWD", "ORCH_HARNESS_FIXED_HEAD",
    "ORCH_HARNESS_PROVIDER", "ORCH_HARNESS_MODEL", "ORCH_HARNESS_EFFORT",
    "ORCH_HARNESS_PROVIDER_BIN", "ORCH_HARNESS_REVIEW_OUTPUT_PATH",
    "ORCH_HARNESS_ORCH_BIN", "ORCH_HARNESS_DEADLINE_SECS",
]
with open(os.environ["B294_CAPTURE"], "w", encoding="utf-8") as stream:
    json.dump({key: os.environ[key] for key in KEYS if key in os.environ}, stream)
count = 0
try:
    with open(os.environ["B294_COUNT"], "r", encoding="utf-8") as stream:
        count = int(stream.read().strip() or "0")
except (OSError, ValueError):
    pass
with open(os.environ["B294_COUNT"], "w", encoding="utf-8") as stream:
    stream.write(str(count + 1))
"#
}

fn pi_stub(root: &Path) -> PathBuf {
    let source = format!(
        "#!/usr/bin/env python3\n{}\nimport sys\nmodel_index = sys.argv.index('--model') + 1\nwith open(os.environ['B294_MODEL_CAPTURE'], 'w', encoding='utf-8') as stream:\n    stream.write(sys.argv[model_index])\nprint('{{\"type\":\"session\",\"id\":\"b294-pi\"}}', flush=True)\nprint('{{\"type\":\"agent_settled\"}}', flush=True)\n",
        provider_capture_prelude()
    );
    executable(&root.join("bin/pi-b294"), source.as_bytes())
}

fn zcode_stub(root: &Path) -> PathBuf {
    let source = r#"#!/usr/bin/env node
const fs = require("fs");
const keys = [
  "ORCH_HARNESS_ID", "ORCH_HARNESS_ACTION_ID", "ORCH_HARNESS_WAKE_ID",
  "ORCH_HARNESS_ROUND", "ORCH_HARNESS_TASK_ID", "ORCH_HARNESS_ATTEMPT_ID",
  "ORCH_HARNESS_ROLE", "ORCH_HARNESS_CWD", "ORCH_HARNESS_FIXED_HEAD",
  "ORCH_HARNESS_PROVIDER", "ORCH_HARNESS_MODEL", "ORCH_HARNESS_EFFORT",
  "ORCH_HARNESS_PROVIDER_BIN", "ORCH_HARNESS_REVIEW_OUTPUT_PATH",
  "ORCH_HARNESS_ORCH_BIN", "ORCH_HARNESS_DEADLINE_SECS"
];
const seen = {};
for (const key of keys) if (process.env[key] !== undefined) seen[key] = process.env[key];
fs.writeFileSync(process.env.B294_CAPTURE, JSON.stringify(seen));
let count = 0;
try { count = Number(fs.readFileSync(process.env.B294_COUNT, "utf8")) || 0; } catch (_) {}
fs.writeFileSync(process.env.B294_COUNT, String(count + 1));
console.log(JSON.stringify({sessionId: "b294-zcode", response: "ok", usage: {totalTokens: 1}}));
"#;
    executable(&root.join("bin/zcode-b294.cjs"), source.as_bytes())
}

fn dsh_stub(root: &Path) -> PathBuf {
    let source = format!(
        r#"#!/usr/bin/env python3
{}
import subprocess
import sys
import time

patch_path = sys.argv[sys.argv.index("--patch") + 1]
with open(patch_path, "r", encoding="utf-8") as stream:
    patch = json.load(stream)
session_root = next(
    entry["config"]["root"]
    for entry in patch
    if entry.get("id") == "session-persistence-jsonl"
)
cwd = os.path.realpath(os.getcwd())
slug = "--" + cwd.strip(os.sep).replace(os.sep, "-") + "--"
session_id = "session-b294-envelope"
session_dir = os.path.join(session_root, slug, session_id)
os.makedirs(session_dir, exist_ok=False)
records = [
    {{"type": "session", "id": session_id, "cwd": cwd}},
    {{"type": "turn/start", "data": {{"turn": 1}}}},
    {{"type": "turn/end", "data": {{"turn": 1}}}},
]
payload = "".join(json.dumps(record, separators=(",", ":")) + "\n" for record in records).encode()
compressed = subprocess.run(
    [os.environ["B294_ZSTD"], "-q", "-c", "-"],
    input=payload,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    check=True,
).stdout
with open(os.path.join(session_dir, "session.jsonl.zstd"), "wb") as stream:
    stream.write(compressed)
    stream.flush()
    os.fsync(stream.fileno())
time.sleep(0.1)
"#,
        provider_capture_prelude()
    );
    executable(&root.join("bin/dsh-b294.py"), source.as_bytes())
}

fn write_zcode_config(root: &Path) -> PathBuf {
    let path = root.join("zcode-config.json");
    fs::write(
        &path,
        serde_json::to_vec(&json!({
            "model": "fixture-provider/fixture-model",
            "provider": {
                "fixture-provider": {
                    "models": {
                        "fixture-model": {
                            "reasoning": {
                                "levels": ["high"],
                                "defaultLevel": "high",
                                "providerOptionsByLevel": {"high": {}}
                            }
                        }
                    }
                }
            }
        }))
        .expect("serialize zcode config"),
    )
    .expect("write zcode config");
    path
}

fn wrapper_message(root: &Path) -> String {
    format!(
        "ORCH REVIEW SITE (B294 fixture)\nWORKTREE={}\nCARGO_TARGET_DIR={}\nREVIEWED_HEAD=4d2ca5a800371c67158bdabc0e0fec10a993eee7\n",
        root.join("site").display(),
        root.join("runtime-target").display()
    )
}

fn wrapper_harness(wrapper: &str) -> &'static str {
    match wrapper {
        PI_WRAPPER => "pi",
        ZCODE_WRAPPER => "zcode",
        DSH_WRAPPER => "dsh",
        other => panic!("unknown managed wrapper: {other}"),
    }
}

fn wrapper_stub(root: &Path, wrapper: &str) -> PathBuf {
    match wrapper {
        PI_WRAPPER => pi_stub(root),
        ZCODE_WRAPPER => zcode_stub(root),
        DSH_WRAPPER => dsh_stub(root),
        other => panic!("unknown managed wrapper: {other}"),
    }
}

fn base_command(root: &Path, wrapper: &str, capture: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .arg(repo_root().join(wrapper))
        .arg(wrapper_message(root))
        .current_dir(root)
        .env("B294_CAPTURE", capture)
        .env(
            "B294_MODEL_CAPTURE",
            root.join(format!("selected-model-{}.txt", wrapper_harness(wrapper))),
        )
        .env("B294_COUNT", root.join("stub-count.txt"))
        .env("B294_ZSTD", find_executable("zstd"))
        .env("ORCH_DSH_ZSTD_BIN", find_executable("zstd"))
        .env("DSH_HOME", root.join("dsh-home"))
        .env("ORCH_ZCODE_CONFIG", write_zcode_config(root))
        .env("ORCH_ZCODE_ROLLOUT_DIR", root.join("rollout"));
    for key in orch_host::harness::ENVELOPE_KEYS {
        command.env_remove(key);
    }
    command
}

fn output_to_raw(output: Output) -> RawOutcome {
    RawOutcome {
        exit_code: output.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn run_with_envelope(
    root: &Path,
    wrapper: &str,
    envelope: &BTreeMap<String, String>,
    alias: Option<(&str, &str)>,
) -> (RawOutcome, BTreeMap<String, String>) {
    let capture = root.join(format!("capture-{}.json", wrapper_harness(wrapper)));
    let mut effective = envelope.clone();
    effective.insert(
        "ORCH_HARNESS_ID".to_string(),
        wrapper_harness(wrapper).to_string(),
    );
    if effective.contains_key("ORCH_HARNESS_PROVIDER_BIN") {
        effective.insert(
            "ORCH_HARNESS_PROVIDER_BIN".to_string(),
            wrapper_stub(root, wrapper).display().to_string(),
        );
    }
    let mut command = base_command(root, wrapper, &capture);
    command.envs(&effective);
    if let Some((key, value)) = alias {
        command.env(key, value);
    }
    let output = command.output().expect("spawn production wrapper");
    let observed = fs::read(&capture)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    (output_to_raw(output), observed)
}

pub fn spawn_wrapper_env(
    root: &Path,
    wrapper: &str,
    envelope: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let (outcome, observed) = run_with_envelope(root, wrapper, envelope, None);
    assert_eq!(
        outcome.exit_code, 0,
        "wrapper {wrapper} failed: {}",
        outcome.stderr
    );
    observed
}

pub fn spawn_wrapper_with_stub(
    root: &Path,
    wrapper: &str,
    envelope: &BTreeMap<String, String>,
) -> Vec<String> {
    spawn_wrapper_env(root, wrapper, envelope)
        .into_keys()
        .collect()
}

pub fn envelope_without_provider_binary(root: &Path) -> BTreeMap<String, String> {
    let mut envelope = valid_envelope(root);
    envelope.insert("ORCH_HARNESS_ID".to_string(), "dsh".to_string());
    envelope.remove("ORCH_HARNESS_PROVIDER_BIN");
    envelope
}

pub fn plant_decoy_on_path(root: &Path, name: &str) -> PathBuf {
    let source = format!(
        "#!/bin/sh\nprintf '1' > '{}'\nexit 0\n",
        root.join("stub-count.txt").display()
    );
    executable(&root.join("decoy-bin").join(name), source.as_bytes())
}

pub fn spawn_wrapper_raw(
    root: &Path,
    wrapper: &str,
    envelope: &BTreeMap<String, String>,
    decoy: &Path,
) -> RawOutcome {
    let capture = root.join("capture-raw.json");
    let mut command = base_command(root, wrapper, &capture);
    command.envs(envelope);
    let mut paths = vec![decoy.parent().expect("decoy parent").to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    command.env(
        "PATH",
        std::env::join_paths(paths).expect("join decoy PATH"),
    );
    output_to_raw(command.output().expect("spawn raw production wrapper"))
}

pub fn stub_execution_count(root: &Path) -> u64 {
    fs::read_to_string(root.join("stub-count.txt"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

pub fn nonexistent_path(root: &Path) -> String {
    root.join("bin/does-not-exist").display().to_string()
}

pub fn plant_unexecutable_file(root: &Path) -> String {
    let path = root.join("bin/unexecutable-orch");
    fs::write(&path, b"not executable\n").expect("write unexecutable fixture");
    let mut permissions = fs::metadata(&path)
        .expect("stat unexecutable fixture")
        .permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&path, permissions).expect("set unexecutable fixture mode");
    path.display().to_string()
}

pub fn spawn_wrapper_with_conflicting_alias(
    root: &Path,
    wrapper: &str,
    envelope: &BTreeMap<String, String>,
    alias: (&str, &str),
) -> AliasOutcome {
    let (outcome, _observed) = run_with_envelope(root, wrapper, envelope, Some(alias));
    assert_eq!(outcome.exit_code, 0, "wrapper conflict scene failed");
    AliasOutcome {
        observed_model: fs::read_to_string(
            root.join(format!("selected-model-{}.txt", wrapper_harness(wrapper))),
        )
        .ok(),
        stderr: outcome.stderr,
    }
}

pub fn spawn_wrapper_legacy_only(root: &Path, wrapper: &str) -> RawOutcome {
    let capture = root.join("capture-legacy.json");
    let mut command = base_command(root, wrapper, &capture);
    match wrapper {
        DSH_WRAPPER => {
            command.env("ORCH_DSH_BIN", dsh_stub(root));
        }
        PI_WRAPPER => {
            command
                .env("ORCH_PI_BIN", pi_stub(root))
                .env("ORCH_PI_CWD", root.join("site"))
                .env("ORCH_PI_PROVIDER", "fixture-provider")
                .env("ORCH_PI_MODEL", "fixture-model")
                .env("ORCH_PI_EFFORT", "high");
        }
        ZCODE_WRAPPER => {
            command
                .env("ORCH_ZCODE_BIN", zcode_stub(root))
                .env("ORCH_ZCODE_CWD", root.join("site"));
        }
        other => panic!("unknown managed wrapper: {other}"),
    }
    output_to_raw(command.output().expect("spawn legacy production wrapper"))
}

pub fn write_minimal_registry(root: &Path) {
    fs::create_dir_all(root.join("coordination/modes")).expect("create fixture modes");
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r77\n")
        .expect("write current round");
    fs::write(
        root.join("coordination/modes/test.yaml"),
        "budgets: {round: {maxModelWakes: 10}}\n",
    )
    .expect("write fixture budget");
    fs::write(
        root.join("coordination/agents.yaml"),
        r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  worker:
    injectable: true
    sessionId: fixture-session
    wake: {argv: ["/bin/sh", "-c", "exit 0", "{message}"]}
"#,
    )
    .expect("write fixture agent registry");
    fs::write(
        root.join("coordination/harnesses.yaml"),
        r#"apiVersion: orch/v1alpha1
kind: HarnessRegistry
metadata: {name: b294-fixture}
harnesses:
  worker:
    harness: pi
    transport: cli-stream
    wrapper: null
    pinSurface: env
    pinTransport: env
    receipt: absent
    terminal: absent
    activity: absent
    control: {status: false, cancel: false, attach: false, reissue: false, declareDead: false}
    modelTruth: fixture
    notes: fixture
"#,
    )
    .expect("write fixture harness registry");
    orch_host::ledger::append(
        root,
        "r77",
        &[orch_host::ledger::event(
            "RoundOpened",
            "runtime:orch",
            None,
            Some("r77"),
            json!({}),
        )],
    )
    .expect("append fixture RoundOpened");
}

pub fn wake_issued_payload(root: &Path) -> Value {
    orch_host::wake::run_wake(root, "worker", "B294 digest fixture")
        .expect("production wake must append WakeIssued");
    let ledger = orch_core::read_ledger(&root.join("coordination/rounds/r77/events.jsonl"))
        .expect("read fixture ledger");
    ledger
        .events
        .iter()
        .find(|event| event.kind == "WakeIssued")
        .and_then(|event| event.payload.clone())
        .expect("production wake must carry a payload")
}
