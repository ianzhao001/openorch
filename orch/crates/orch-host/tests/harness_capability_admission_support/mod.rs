//! Scratch inputs for the frozen B293 harness admission contract.
//!
//! This helper only creates YAML scenes and mutates their source bytes. All
//! admission decisions are made by the production APIs exercised by the seed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCENE: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR should be nested under the repository root")
        .to_path_buf()
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).expect("fixture scalar should serialize")
}

/// Create a process-unique and thread-unique scratch root inside the repository.
pub fn scratch_root(tag: &str) -> PathBuf {
    let sequence = NEXT_SCENE.fetch_add(1, Ordering::Relaxed);
    let root = repo_root()
        .join("orch/target/test-tmp")
        .join(format!("b293-{tag}-{}-{sequence}", std::process::id()));
    fs::create_dir_all(root.join("coordination")).expect("fixture coordination directory");
    root
}

/// Write a minimal valid legacy agent registry for the supplied exact keys.
pub fn write_agents(root: &Path, agents: &[&str]) {
    let mut yaml = String::from("apiVersion: orch/v1alpha1\nkind: AgentRegistry\nagents:\n");
    for agent in agents {
        yaml.push_str(&format!(
            "  {}:\n    injectable: true\n    sessionId: fixture\n    wake: {{argv: [\"echo\", \"{{message}}\"]}}\n",
            yaml_scalar(agent)
        ));
    }
    fs::write(root.join("coordination/agents.yaml"), yaml).expect("write fixture agents.yaml");
}

/// Write one valid legacy agent, optionally declaring a signed model pin.
pub fn write_agents_with_pin(root: &Path, agent: &str, model: Option<&str>) {
    let mut yaml = format!(
        "apiVersion: orch/v1alpha1\nkind: AgentRegistry\nagents:\n  {}:\n    injectable: true\n    sessionId: fixture\n",
        yaml_scalar(agent)
    );
    if let Some(model) = model {
        yaml.push_str(&format!(
            "    model: {}\n    observation: {{source: fixture, policy: advisory}}\n",
            yaml_scalar(model)
        ));
    }
    yaml.push_str("    wake: {argv: [\"echo\", \"{message}\"]}\n");
    fs::write(root.join("coordination/agents.yaml"), yaml).expect("write pinned agents.yaml");
}

/// Write complete capability descriptors while allowing selected enum spellings to be invalid.
pub fn write_harnesses(root: &Path, entries: &[(&str, &str, &str)]) {
    let mut yaml = String::from(
        "apiVersion: orch/v1alpha1\nkind: HarnessRegistry\nmetadata:\n  name: b293-fixture\n  updatedAt: \"2026-08-22\"\n  originRound: fixture\nharnesses:\n",
    );
    for (agent, harness, pin_transport) in entries {
        yaml.push_str(&format!(
            "  {}:\n    harness: {}\n    transport: cli-stream\n    wrapper: null\n    pinSurface: env\n    pinTransport: {}\n    receipt: native\n    terminal: derived\n    activity: derived\n    control: {{status: false, cancel: false, attach: false, reissue: false, declareDead: false}}\n    modelTruth: fixture\n    notes: fixture\n",
            yaml_scalar(agent),
            yaml_scalar(harness),
            yaml_scalar(pin_transport)
        ));
    }
    fs::write(root.join("coordination/harnesses.yaml"), yaml)
        .expect("write fixture harnesses.yaml");
}

/// Append semantically inert bytes so the digest test can detect exact-byte changes.
pub fn append_comment_line(root: &Path) {
    let path = root.join("coordination/harnesses.yaml");
    let mut bytes = fs::read(&path).expect("read fixture harnesses.yaml");
    bytes.extend_from_slice(b"# digest-only comment\n");
    fs::write(path, bytes).expect("append descriptor comment");
}
