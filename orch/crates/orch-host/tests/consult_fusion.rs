//! B166 seeded-red contract (H30: every member works in its own in-repo site,
//! and one dead member never kills the fusion).
//!
//! Negative mutations that must turn the named case red:
//! M1. Put a consult site anywhere outside the repo root, or let two members of
//!     the same run share a path. /tmp is unreadable to opencode's sandbox (the
//!     r53 incident), and a self-fusion preset runs the same adapter three times
//!     — colliding paths would silently merge three fusionlists into one.
//! M2. Make any single member's failure fatal, or collapse a timeout into a
//!     generic failure. Partial failure is the designed behaviour; a slow member
//!     must not be able to erase its peers' answers.
//! M3. Spawn before archiving the outgoing bytes, or reorder results away from
//!     the input order. Every byte that leaves the repo must be on disk first,
//!     and self-fusion needs stable indices to tell its three runs apart.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use orch_host::consult::{run_consultation, ConsultArgs, JudgeMode, MemberStatus};
use orch_host::fusion::{consult_site_plan, run_fusion, FusionMember};
use orch_host::plan::task_validated_payload;

mod support_worktree;

use support_worktree::scoped_worktrees;

static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);
static FUSION_TEST_LOCK: Mutex<()> = Mutex::new(());
const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn temp_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root.join("target/test-tmp").join(format!(
        "consult-fusion-{name}-{}-{}-{seq}",
        std::process::id(),
        ulid::Ulid::new()
    ))
}

fn write_fake_adapter(root: &Path, name: &str, lines: Vec<String>) {
    let mut argv = vec![
        serde_json::Value::String("/bin/sh".to_string()),
        serde_json::Value::String("-c".to_string()),
        serde_json::Value::String("printf '%s\\n' \"$@\"".to_string()),
        serde_json::Value::String(name.to_string()),
    ];
    argv.extend(lines.into_iter().map(serde_json::Value::String));
    let spec = serde_json::json!({
        "launch": {
            "argv": argv,
            "cwd_is_workdir": true,
        }
    });
    let dir = root.join("coordination/adapters");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{name}.yaml")),
        serde_yaml::to_string(&spec).unwrap(),
    )
    .unwrap();
}

fn provider_fixture_root() -> PathBuf {
    let root = temp_root("provider-shapes");
    fs::create_dir_all(root.join("coordination/runtime")).unwrap();
    fs::create_dir_all(root.join("coordination/rounds/r90")).unwrap();
    fs::create_dir_all(root.join("coordination/consult")).unwrap();
    fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r90\n").unwrap();
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        "data:\n  forbiddenArtifactPatterns: ['*.pem', '*.key', '.env*']\n",
    )
    .unwrap();
    let ledger = serde_json::json!({
        "eventId": "01B168PROVIDERSHAPES0001",
        "ts": "2026-07-29T00:00:00Z",
        "actor": "runtime:orch",
        "type": "TaskValidated",
        "round": "r90",
        "payload": task_validated_payload(1, DIGEST),
    });
    fs::write(
        root.join("coordination/rounds/r90/events.jsonl"),
        format!("{ledger}\n"),
    )
    .unwrap();
    fs::write(
        root.join("coordination/consult/presets.yaml"),
        "apiVersion: orch/v1alpha1\nkind: ConsultPresets\ndefaults: {perMemberTimeoutSecs: 5, totalWallSecs: 30, maxMembers: 8}\npresets:\n  - name: provider-shapes\n    fusion: [consult-codex, consult-opencode, consult-claude, consult-agy, consult-unknown]\n",
    )
    .unwrap();
    fs::write(
        root.join("question.md"),
        "How should answers be normalised?\n",
    )
    .unwrap();

    let mut codex_lines =
        vec![r#"{"type":"thread.started","thread_id":"t1","model":"gpt-5.6-codex"}"#.to_string()];
    codex_lines.extend((0..400).map(|seq| {
        serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "command_execution",
                "aggregated_output": "x".repeat(200),
                "seq": seq,
            }
        })
        .to_string()
    }));
    codex_lines.push(
        r#"{"type":"item.completed","item":{"type":"agent_message","text":"CODEX PRODUCTION FINAL"}}"#
            .to_string(),
    );
    write_fake_adapter(&root, "consult-codex", codex_lines);
    write_fake_adapter(
        &root,
        "consult-opencode",
        vec![
            r#"{"type":"step_start","part":{"type":"step-start","modelID":"glm-5.2"}}"#.to_string(),
            r#"{"type":"tool_use","part":{"type":"tool","state":{"output":"noise"}}}"#.to_string(),
            r#"{"type":"text","part":{"type":"text","text":"OPENCODE PRODUCTION FINAL"}}"#
                .to_string(),
            r#"{"type":"step_finish","part":{"tokens":{"input":1,"output":2}}}"#.to_string(),
        ],
    );
    write_fake_adapter(
        &root,
        "consult-claude",
        vec![
            r#"{"type":"system","subtype":"init","model":"claude-opus-4-6"}"#.to_string(),
            r#"{"type":"result","result":"CLAUDE PRODUCTION FINAL"}"#.to_string(),
        ],
    );
    write_fake_adapter(
        &root,
        "consult-agy",
        vec!["AGY PRODUCTION FINAL".to_string()],
    );
    write_fake_adapter(
        &root,
        "consult-unknown",
        vec![format!(
            r#"{{"type":"unknown_shape","payload":"{}"}}"#,
            "x".repeat(80 * 1024)
        )],
    );
    root
}

const CONSULT_ID: &str = "01JCONSULTSEEDFIXTURE0001";

#[test]
fn every_member_gets_its_own_site_inside_the_repo() {
    // M1: repo-internal and index-distinct, so self-fusion cannot collapse.
    let a = consult_site_plan("/repo/root", CONSULT_ID, 0, "consult-codex").expect("site plan");
    let b = consult_site_plan("/repo/root", CONSULT_ID, 1, "consult-codex").expect("site plan");
    for site in [&a, &b] {
        assert!(
            site.worktree.starts_with("/repo/root/"),
            "worktree must live inside the repo: {}",
            site.worktree
        );
        assert!(
            site.target_dir.starts_with("/repo/root/"),
            "build dir must live inside the repo: {}",
            site.target_dir
        );
        for banned in ["/tmp", "/private/tmp", "/var/folders"] {
            assert!(!site.worktree.starts_with(banned));
            assert!(!site.target_dir.starts_with(banned));
        }
    }
    assert_ne!(
        a.worktree, b.worktree,
        "same adapter, different index, different site"
    );
    assert_ne!(a.target_dir, b.target_dir);
    // Idempotent: the same identity always plans to the same place.
    let again = consult_site_plan("/repo/root", CONSULT_ID, 0, "consult-codex").unwrap();
    assert_eq!(a.worktree, again.worktree);
    // Escapes are refused by name.
    assert!(consult_site_plan("", CONSULT_ID, 0, "consult-codex").is_err());
    assert!(consult_site_plan("/repo/root", CONSULT_ID, 0, "../escape").is_err());
}

#[test]
fn one_dead_member_does_not_kill_the_fusion_and_a_slow_one_is_its_own_state() {
    let _serial = FUSION_TEST_LOCK.lock().unwrap();
    // M2: three fake adapters — ok / non-zero exit / sleeps past its deadline.
    let root = temp_root("partial");
    let members = vec![
        FusionMember::new(0, "seed-ok"),
        FusionMember::new(1, "seed-boom"),
        FusionMember::new(2, "seed-slow"),
    ];
    let outcomes = run_fusion(&root, &members, "question", seed_limits())
        .expect("a partially failing fusion still returns");

    assert_eq!(outcomes.len(), 3, "every member gets an outcome slot");
    assert_eq!(outcomes[0].status, MemberStatus::Ok);
    assert!(outcomes[0]
        .answer
        .as_deref()
        .unwrap_or_default()
        .contains("seed-ok"));
    assert_eq!(outcomes[1].status, MemberStatus::Failed);
    assert!(
        outcomes[1].failure_class.is_some(),
        "a failed member carries a structured class, not prose"
    );
    assert_eq!(
        outcomes[2].status,
        MemberStatus::TimedOut,
        "a timeout is its own state, never folded into Failed"
    );
    assert!(
        outcomes.iter().any(|o| o.status == MemberStatus::Ok),
        "one survivor is enough for the fusion to continue"
    );
}

#[test]
fn outgoing_bytes_are_archived_before_the_spawn_and_order_is_stable() {
    let _serial = FUSION_TEST_LOCK.lock().unwrap();
    // M3: the archive is a precondition of the spawn, and indices are stable.
    let root = temp_root("archive");
    let members = vec![
        FusionMember::new(0, "seed-echo-archive"),
        FusionMember::new(1, "seed-ok"),
    ];
    let outcomes = run_fusion(&root, &members, "question", seed_limits()).expect("fusion runs");

    // seed-echo-archive prints back whether its own request archive already
    // existed when it started — the only way to prove ordering from outside.
    assert!(
        outcomes[0]
            .answer
            .as_deref()
            .unwrap_or_default()
            .contains("ARCHIVE_PRESENT=1"),
        "the member's request bytes must be on disk before it is spawned"
    );
    assert_eq!(outcomes[0].index, 0);
    assert_eq!(outcomes[1].index, 1);
    assert_eq!(outcomes[0].member, "seed-echo-archive");
    assert_eq!(outcomes[1].member, "seed-ok");
}

#[test]
fn real_provider_shapes_flow_through_fusion_meta_and_cleanup() {
    let _serial = FUSION_TEST_LOCK.lock().unwrap();
    let root = provider_fixture_root();
    let outcome = run_consultation(
        &root,
        &ConsultArgs {
            question: root.join("question.md"),
            preset: "provider-shapes".to_string(),
            judge: JudgeMode::Planner,
            ..ConsultArgs::default()
        },
    )
    .expect("provider-shaped fake adapters should complete");
    let consult_worktree_prefix = format!("consult-{}-", outcome.id);
    assert!(
        scoped_worktrees(&root, &consult_worktree_prefix).is_empty(),
        "this consultation must clean all of its own worktrees"
    );

    let answers = outcome
        .members
        .iter()
        .map(|member| member.answer.as_deref().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(answers[0], "CODEX PRODUCTION FINAL");
    assert_eq!(answers[1], "OPENCODE PRODUCTION FINAL");
    assert_eq!(answers[2], "CLAUDE PRODUCTION FINAL");
    assert_eq!(answers[3], "AGY PRODUCTION FINAL\n");
    assert!(!answers[0].contains("\"type\""));
    assert!(!answers[1].contains("\"type\""));
    assert!(!answers[2].contains("\"type\""));

    let extraction = outcome
        .members
        .iter()
        .map(|member| member.answer_extraction.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(
        extraction,
        vec![
            Some("structured"),
            Some("structured"),
            Some("structured"),
            Some("plain-text"),
            Some("raw-transcript"),
        ]
    );
    assert_eq!(
        outcome.members[0].observed_model.as_deref(),
        Some("gpt-5.6-codex")
    );
    assert_eq!(
        outcome.members[1].observed_model.as_deref(),
        Some("glm-5.2")
    );
    assert_eq!(
        outcome.members[2].observed_model.as_deref(),
        Some("claude-opus-4-6")
    );
    assert_eq!(
        outcome.members[3].observed_model, None,
        "plain output must not become guessed model evidence"
    );

    let meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(outcome.dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["members"][0]["answerExtraction"], "structured");
    assert_eq!(meta["members"][3]["answerExtraction"], "plain-text");
    assert_eq!(meta["members"][4]["answerExtraction"], "raw-transcript");
    assert!(meta["members"][3]["observedModel"].is_null());

    let codex_raw_log = fs::read(
        outcome
            .dir
            .join("adapter-logs/member-0-consult-codex.jsonl"),
    )
    .unwrap();
    assert!(
        codex_raw_log.len() > 64 * 1024,
        "production path fixture must carry a large provider log"
    );
    assert!(
        answers[0].len() < 64 * 1024,
        "the extracted terminal answer itself remains small"
    );

    let prompt = fs::read_to_string(outcome.dir.join("judge-prompt.md")).unwrap();
    assert!(prompt.contains("OPENCODE PRODUCTION FINAL"));
    assert!(prompt.contains("已截断，完整见 fusion/4-consult-unknown.md"));
    assert!(
        prompt.len() < 80 * 1024,
        "the 80 KiB raw transcript must be bounded in the judge prompt"
    );
    let raw_log = fs::read(
        outcome
            .dir
            .join("adapter-logs/member-4-consult-unknown.jsonl"),
    )
    .unwrap();
    assert!(
        raw_log.len() > 64 * 1024,
        "the full raw transcript remains available for audit"
    );
}

fn seed_limits() -> orch_host::consult::ConsultLimits {
    orch_host::consult::ConsultLimits {
        per_member_timeout_secs: 5,
        total_wall_secs: 30,
        max_members: 8,
    }
}
