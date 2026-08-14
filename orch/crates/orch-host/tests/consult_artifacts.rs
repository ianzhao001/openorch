//! B167 seeded-red contract (H30: the gate guards the driver's front door, a
//! dead judge never erases the fusion, and the default judge mode spawns nobody).
//!
//! Negative mutations that must turn the named case red:
//! M1. Skip the gate at the driver entry, so a signed-off round consults anyway.
//!     The gate is only worth anything if it sits before the first spawn.
//! M2. Let a judge failure fail the whole consultation and drop the fusion
//!     answers. The fusion's raw output is the expensive part; the synthesis is
//!     a convenience on top of it (this mirrors the official analyst semantics).
//! M3. Spawn a judge when none was asked for. `--judge planner` is the default
//!     and must cost zero subprocesses: orch only lays down the prompt and the
//!     material for the planner session to answer.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::consult::{run_consultation, ConsultArgs, JudgeMode};
use orch_host::judge::{parse_judge_sections, JudgeStatus};
use orch_host::plan::task_validated_payload;

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root.join("target/test-tmp").join(format!(
        "consult-artifacts-{name}-{}-{}-{seq}",
        std::process::id(),
        ulid::Ulid::new()
    ))
}

const JUDGE_TEXT: &str = "## Consensus\nc\n## Contradictions\nd\n## Partial coverage\np\n## Unique insights\nu\n## Blind spots\nb\n";

/// Lays down CURRENT-ROUND, a ledger, a presets file wired to seed fake
/// adapters, and the question. `signed_off` decides which side of the gate the
/// fixture sits on.
fn fixture_root(name: &str, signed_off: bool) -> PathBuf {
    let root = temp_root(name);
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r90")).unwrap();
    fs::create_dir_all(root.join("coordination/consult")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();

    let mut ledger = serde_json::json!({
        "eventId": "01SEEDART0000000000000001", "ts": "2026-07-29T00:00:00Z",
        "actor": "runtime:orch", "type": "TaskValidated", "round": "r90",
        "payload": task_validated_payload(1, DIGEST),
    })
    .to_string();
    ledger.push('\n');
    if signed_off {
        ledger.push_str(
            &serde_json::json!({
                "eventId": "01SEEDART0000000000000002", "ts": "2026-07-29T00:00:01Z",
                "actor": "user", "type": "PlanSignedOff", "round": "r90",
                "payload": {"irRevision": 1, "validationDigest": DIGEST},
            })
            .to_string(),
        );
        ledger.push('\n');
    }
    fs::write(root.join("coordination/rounds/r90/events.jsonl"), ledger).unwrap();
    fs::write(
        root.join("coordination/consult/presets.yaml"),
        "apiVersion: orch/v1alpha1\nkind: ConsultPresets\ndefaults: {perMemberTimeoutSecs: 5, totalWallSecs: 30, maxMembers: 8}\npresets:\n  - name: default\n    fusion: [seed-ok, seed-ok2]\n",
    )
    .unwrap();
    fs::write(
        root.join("question.md"),
        "How should we shard the ledger?\n",
    )
    .unwrap();
    root
}

fn args(root: &Path, judge: JudgeMode) -> ConsultArgs {
    ConsultArgs {
        question: root.join("question.md"),
        preset: "default".to_string(),
        judge,
        ..ConsultArgs::default()
    }
}

#[test]
fn a_signed_off_round_refuses_at_the_driver_entry() {
    // M1: the gate is the driver's first act; nothing may be spawned after it
    // refuses, and the refusal is recorded.
    let root = fixture_root("gate", true);
    let err = run_consultation(&root, &args(&root, JudgeMode::Planner)).unwrap_err();
    assert!(
        err.to_string().contains("PlanSignedOff"),
        "the refusal names the freeze: {err}"
    );
    assert!(
        !root
            .join("coordination/consultations")
            .join("fusion")
            .exists(),
        "no consultation dir is materialised on refusal"
    );
    let log = fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
    assert_eq!(log.trim().lines().count(), 1);
    assert!(log.contains("ConsultationRefused"));
}

#[test]
fn a_dead_judge_keeps_the_raw_fusion_answers() {
    // M2: mirror the official analyst semantics — the analysis may be missing,
    // the responses never are.
    let root = fixture_root("judge", false);
    let outcome = run_consultation(&root, &args(&root, JudgeMode::Adapter("seed-boom".into())))
        .expect("a dead judge degrades, it does not kill the consultation");
    assert_eq!(outcome.judge_status, JudgeStatus::Failed);
    assert!(
        !outcome.dir.join("judge.md").exists(),
        "no fabricated synthesis is written"
    );
    let fusion: Vec<_> = fs::read_dir(outcome.dir.join("fusion")).unwrap().collect();
    assert_eq!(
        fusion.len(),
        2,
        "both fusion answers survive the judge failure"
    );
    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(outcome.dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["judge"]["status"], "failed");
    // Sanity: the parser accepts a complete five-section synthesis.
    assert!(parse_judge_sections(JUDGE_TEXT).complete);
    assert!(!parse_judge_sections("## Consensus\nonly one\n").complete);
}

#[test]
fn the_default_planner_judge_spawns_nothing_and_the_log_only_appends() {
    // M3: `--judge planner` is the default and must cost zero subprocesses.
    let root = fixture_root("planner-judge", false);
    let first = run_consultation(&root, &args(&root, JudgeMode::Planner)).expect("fusion runs");
    assert_eq!(first.judge_status, JudgeStatus::Planner);
    assert!(
        first.dir.join("judge-prompt.md").exists(),
        "the planner is handed a prompt, not a verdict"
    );
    assert!(!first.dir.join("judge.md").exists());
    assert_eq!(
        first.judge_spawns, 0,
        "planner mode must not spawn any judge subprocess"
    );
    for artifact in ["request/question.md", "request/prompt.md", "meta.json"] {
        assert!(
            first.dir.join(artifact).exists(),
            "missing {artifact} in {}",
            first.dir.display()
        );
    }

    let log_after_first =
        fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
    let second = run_consultation(&root, &args(&root, JudgeMode::Planner)).expect("fusion runs");
    assert_ne!(
        first.dir, second.dir,
        "each consultation gets its own ULID dir"
    );
    let log = fs::read_to_string(root.join("coordination/consultations/log.jsonl")).unwrap();
    assert_eq!(log.trim().lines().count(), 2);
    assert!(log.starts_with(&log_after_first), "the log is append-only");
}
