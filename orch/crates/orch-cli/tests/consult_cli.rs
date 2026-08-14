//! B167 CLI integration: the planning-only consultation front door returns a
//! real non-zero process status for frozen rounds and rejects bad configuration
//! or unsafe attachments before any adapter can be spawned.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
static ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn fixture_orch_command(extras: &[(&str, &str)]) -> Command {
    let mut command = Command::new(support::orch_bin());
    support::configure_fixture_git_env(&mut command, extras);
    command
}

fn temp_root(name: &str, signed_off: bool) -> PathBuf {
    let orch_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("orch workspace root");
    let seq = ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = orch_dir
        .join("target/test-tmp")
        .join(format!("consult-cli-{name}-{}-{seq}", std::process::id()));
    for relative in [
        "coordination/runtime",
        "coordination/rounds/r90",
        "coordination/consult",
        "coordination/adapters",
    ] {
        fs::create_dir_all(root.join(relative)).unwrap();
    }
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();

    let mut ledger = format!(
        "{{\"eventId\":\"01CONSULTCLI00000000000001\",\"ts\":\"2026-07-29T00:00:00Z\",\"actor\":\"runtime:orch\",\"type\":\"TaskValidated\",\"round\":\"r90\",\"payload\":{{\"irRevision\":1,\"validationDigest\":\"{DIGEST}\"}}}}\n"
    );
    if signed_off {
        ledger.push_str(&format!(
            "{{\"eventId\":\"01CONSULTCLI00000000000002\",\"ts\":\"2026-07-29T00:00:01Z\",\"actor\":\"user\",\"type\":\"PlanSignedOff\",\"round\":\"r90\",\"payload\":{{\"irRevision\":1,\"validationDigest\":\"{DIGEST}\"}}}}\n"
        ));
    }
    fs::write(root.join("coordination/rounds/r90/events.jsonl"), ledger).unwrap();
    fs::write(
        root.join("coordination/consult/presets.yaml"),
        "apiVersion: orch/v1alpha1\nkind: ConsultPresets\ndefaults: {perMemberTimeoutSecs: 5, totalWallSecs: 30, maxMembers: 2}\npresets:\n  - name: default\n    fusion: [seed-ok]\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/adapters/seed-ok.yaml"),
        "launch:\n  argv: [/bin/echo, ok]\n  cwd_is_workdir: true\n",
    )
    .unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "data:\n  forbiddenArtifactPatterns: ['*.pem', '*.key', '.env*']\n",
    )
    .unwrap();
    fs::write(root.join("question.md"), "How should this plan proceed?\n").unwrap();
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    fixture_orch_command(&[])
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("启动 worktree 默认 target 下的 orch 二进制失败")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn signed_off_round_exits_nonzero_and_names_plan_signed_off() {
    let root = temp_root("signed-off", true);
    let question = root.join("question.md");
    let output = run(&root, &["consult", question.to_str().unwrap()]);
    assert!(!output.status.success(), "signed-off consult must fail");
    assert!(
        stderr(&output).contains("PlanSignedOff"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn unknown_preset_exits_nonzero() {
    let root = temp_root("unknown-preset", false);
    let question = root.join("question.md");
    let output = run(
        &root,
        &["consult", question.to_str().unwrap(), "--preset", "missing"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("未知 consult preset"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn attachment_outside_repository_is_rejected() {
    let root = temp_root("outside", false);
    let question = root.join("question.md");
    let outside = root.parent().unwrap().join(format!(
        "consult-cli-outside-{}-{}.txt",
        std::process::id(),
        ROOT_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&outside, "safe outside text\n").unwrap();
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--attach",
            outside.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("仓外文件"), "{}", stderr(&output));
}

#[test]
fn env_attachment_is_rejected_by_binding_pattern() {
    let root = temp_root("env", false);
    let question = root.join("question.md");
    let attachment = root.join(".env.local");
    fs::write(&attachment, "SAFE_VALUE=plain\n").unwrap();
    let output = run(
        &root,
        &[
            "consult",
            question.to_str().unwrap(),
            "--attach",
            attachment.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("forbiddenArtifactPatterns")
            && stderr(&output).contains(".env.local"),
        "{}",
        stderr(&output)
    );
}
