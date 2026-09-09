use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::legacy::{
    replay_source_reader_closure_v1 as resolve_source_reader_closure_at_tree_v1,
    SourceReaderClosureDecisionV1, SourceReaderTargetV1,
};
use sha2::{Digest, Sha256};


// B328: materialize owned test input as a Git commit, then exercise the production
// historical auditor. No production API can inspect this mutable fixture directory.
fn fixture_git_text(root: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::process::Stdio;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn resolve_source_reader_closure_v1(
    root: &Path,
    candidate: &Path,
    policy_base: &str,
    changed: &[String],
) -> anyhow::Result<SourceReaderClosureDecisionV1> {
    if candidate == root {
        // This positive case audits the historical baseline, not today's source shape.
        return resolve_source_reader_closure_at_tree_v1(
            root,
            "r82",
            policy_base,
            policy_base,
            changed,
        );
    }
    let audit = scratch("committed-reader-input");
    fs::create_dir_all(audit.parent().unwrap()).unwrap();
    fixture_git_text(
        root,
        &[
            "clone",
            "--quiet",
            "--bare",
            "--shared",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            root.to_str().unwrap(),
            audit.to_str().unwrap(),
        ],
        None,
    );
    fixture_git_text(&audit, &["read-tree", policy_base], None);
    for path in changed {
        let input = candidate.join(path);
        match fs::symlink_metadata(&input) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                let bytes = fs::read(input).unwrap();
                let blob =
                    fixture_git_text(&audit, &["hash-object", "-w", "--stdin"], Some(&bytes));
                fixture_git_text(
                    &audit,
                    &[
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        "100644",
                        &blob,
                        path,
                    ],
                    None,
                );
            }
            _ => {
                fixture_git_text(
                    &audit,
                    &["update-index", "--force-remove", "--", path],
                    None,
                );
            }
        }
    }
    let tree = fixture_git_text(&audit, &["write-tree"], None);
    let commit = fixture_git_text(
        &audit,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit-tree",
            &tree,
            "-p",
            policy_base,
            "-m",
            "immutable reader fixture",
        ],
        None,
    );
    let result =
        resolve_source_reader_closure_at_tree_v1(&audit, "r82", &commit, policy_base, changed);
    fs::remove_dir_all(audit).unwrap();
    result
}

fn authenticated_reader_target_for_test(
    root: &Path,
    policy_base: &str,
    reader: &str,
) -> anyhow::Result<SourceReaderTargetV1> {
    match resolve_source_reader_closure_at_tree_v1(
        root,
        "r82",
        policy_base,
        policy_base,
        &["orch/docs/AI-MECHANICAL-GUIDE.md".to_owned()],
    )? {
        SourceReaderClosureDecisionV1::Closed { targets, .. } => targets
            .into_iter()
            .find(|target| target.reader == reader)
            .ok_or_else(|| anyhow::anyhow!("historical closure lacks {reader}")),
        SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } => anyhow::bail!(reason),
    }
}

#[test]
fn historical_reader_api_refuses_live_schema3_and_movable_refs() {
    let root = repo_root();
    let exact = head(&root);
    let error =
        resolve_source_reader_closure_at_tree_v1(&root, "r84", &exact, &exact, &[]).unwrap_err();
    assert!(
        error.to_string().contains("schema 3 must not enter"),
        "{error:#}"
    );
    assert!(resolve_source_reader_closure_at_tree_v1(&root, "r82", "main", &exact, &[]).is_err());
}

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

const B311_PRE_FLIP_DESCRIPTOR_SHA256: &str =
    "d003a17e81b783952d385c2cd6d082480c84108fff0b8cfff1c851373c67e489";
const B311_POST_FLIP_DESCRIPTOR_SHA256: &str =
    "8f8a4fc69261c42b5831cd7d3fe711034c9779f6184cd3875d7a2bd9eaa1a005";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum B311DescriptorState {
    PreFlip,
    PostFlip,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn head(root: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse 应可启动");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("git SHA 应为 UTF-8")
        .trim()
        .to_string()
}

fn rev(root: &Path, value: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", value])
        .output()
        .expect("git rev-parse should start");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn scratch(tag: &str) -> PathBuf {
    repo_root().join(".cowork-temp").join(format!(
        "b306-source-reader-{tag}-{}-{}",
        std::process::id(),
        SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

fn git_ok(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn classify_b311_descriptor(bytes: &[u8]) -> Result<B311DescriptorState, String> {
    let digest = sha256_hex(bytes);
    match digest.as_str() {
        B311_PRE_FLIP_DESCRIPTOR_SHA256 => Ok(B311DescriptorState::PreFlip),
        B311_POST_FLIP_DESCRIPTOR_SHA256 => Ok(B311DescriptorState::PostFlip),
        _ => Err(format!(
            "B311 reader fixture descriptor has unknown digest: {digest}"
        )),
    }
}

fn parse_event_lines(bytes: &[u8]) -> Result<Vec<serde_json::Value>, String> {
    bytes
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
        .map(|(index, line)| {
            serde_json::from_slice(line)
                .map_err(|error| format!("ledger line {} is malformed: {error}", index + 1))
        })
        .collect()
}

fn is_lower_full_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_dispatch_base_from_events(
    events: &[serde_json::Value],
    round: &str,
    task: &str,
    attempt: &str,
    agent: &str,
) -> Result<String, String> {
    let matches = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(serde_json::Value::as_str) == Some("DispatchIssued")
                && event.get("round").and_then(serde_json::Value::as_str) == Some(round)
                && event.get("taskId").and_then(serde_json::Value::as_str) == Some(task)
                && event
                    .get("payload")
                    .and_then(|payload| payload.get("attemptId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(attempt)
        })
        .collect::<Vec<_>>();
    let [dispatch] = matches.as_slice() else {
        return Err(format!(
            "expected exactly one {round}/{task}/{attempt} DispatchIssued, found {}",
            matches.len()
        ));
    };
    if dispatch.get("actor").and_then(serde_json::Value::as_str) != Some("runtime:orch") {
        return Err("DispatchIssued actor must be runtime:orch".to_string());
    }
    let payload = dispatch
        .get("payload")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "DispatchIssued payload must be an object".to_string())?;
    if payload.get("attemptNo").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err("DispatchIssued attemptNo must be 1".to_string());
    }
    if payload.get("agent").and_then(serde_json::Value::as_str) != Some(agent) {
        return Err(format!("DispatchIssued agent must be {agent}"));
    }
    let base = payload
        .get("baseSha")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "DispatchIssued baseSha is missing".to_string())?;
    if !is_lower_full_sha(base) {
        return Err("DispatchIssued baseSha must be a 40-byte lowercase commit".to_string());
    }
    Ok(base.to_string())
}

fn recorded_dispatch_base(
    root: &Path,
    round: &str,
    task: &str,
    attempt: &str,
    agent: &str,
) -> Result<String, String> {
    let path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let bytes = fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let events = parse_event_lines(&bytes)?;
    canonical_dispatch_base_from_events(&events, round, task, attempt, agent)
}

fn legacy_source_reader_policy_base(root: &Path) -> String {
    recorded_dispatch_base(root, "r82", "B312", "B312-A0001", "executor-desktop")
        .expect("B312 dispatch base is the immutable post-B311 legacy policy fixture")
}

fn switch_to_legacy_source_reader_policy_base(root: &Path, clone: &Path) {
    let base = legacy_source_reader_policy_base(root);
    git_ok(clone, &["switch", "--detach", &base]);
}

fn validate_b311_recorded_owner_history(bytes: &[u8]) -> Result<(), String> {
    let events = parse_event_lines(bytes)?;
    let matches = |kind: &str| {
        events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                event.get("type").and_then(serde_json::Value::as_str) == Some(kind)
                    && event.get("round").and_then(serde_json::Value::as_str) == Some("r82")
                    && event.get("taskId").and_then(serde_json::Value::as_str) == Some("B311")
            })
            .collect::<Vec<_>>()
    };
    let merges = matches("MergeExecuted");
    let recorded = matches("TaskRecorded");
    let [(merge_position, merge)] = merges.as_slice() else {
        return Err(format!(
            "post-flip B311 owner history requires exactly one MergeExecuted, found {}",
            merges.len()
        ));
    };
    let [(recorded_position, recorded)] = recorded.as_slice() else {
        return Err(format!(
            "post-flip B311 owner history requires exactly one TaskRecorded, found {}",
            recorded.len()
        ));
    };
    if merge.get("actor").and_then(serde_json::Value::as_str) != Some("reviewer:orch-runtime")
        || merge
            .get("payload")
            .and_then(|payload| payload.get("policy"))
            .and_then(serde_json::Value::as_str)
            != Some("no-ff")
    {
        return Err("post-flip B311 MergeExecuted envelope is not canonical".to_string());
    }
    let merge_sha = merge
        .get("payload")
        .and_then(|payload| payload.get("mergeSha"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "post-flip B311 MergeExecuted.mergeSha is missing".to_string())?;
    if !is_lower_full_sha(merge_sha) {
        return Err("post-flip B311 MergeExecuted.mergeSha is not canonical".to_string());
    }
    if recorded.get("actor").and_then(serde_json::Value::as_str) != Some("runtime:orch")
        || recorded
            .get("payload")
            .and_then(|payload| payload.get("postMergeGates"))
            .and_then(serde_json::Value::as_str)
            != Some("all-green")
    {
        return Err("post-flip B311 TaskRecorded envelope is not canonical".to_string());
    }
    if merge_position >= recorded_position {
        return Err("post-flip B311 owner history is out of order".to_string());
    }
    Ok(())
}

fn validate_post_flip_binding(binding: &str) -> Result<(), String> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(binding).map_err(|error| format!("parse binding: {error}"))?;
    if value["metadata"]["bindingRevision"].as_u64() == Some(2) {
        if !value["runtimePolicies"].is_null() {
            return Err("schema 3 binding must retire runtimePolicies".to_string());
        }
        return orch_host::binding::validate_v3_binding_shape(binding.as_bytes())
            .map_err(|error| format!("validate schema 3 binding: {error:#}"));
    }
    let policy = &value["runtimePolicies"]["policies"]["candidate-lanes-v1"];
    if policy["ownerTask"].as_str() != Some("B311") {
        return Err("post-flip candidate-lanes-v1 owner must be B311".to_string());
    }
    if policy["sourceReaderClosure"]["sha256"].as_str() != Some(B311_POST_FLIP_DESCRIPTOR_SHA256) {
        return Err("post-flip source-reader descriptor digest is not bound exactly".to_string());
    }
    Ok(())
}

fn b311_reader_fixture(tag: &str) -> PathBuf {
    let root = repo_root();
    let clone = scratch(tag);
    let output = Command::new("git")
        .args(["clone", "--quiet", "--shared"])
        .arg(&root)
        .arg(&clone)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    switch_to_legacy_source_reader_policy_base(&root, &clone);
    let descriptor_path = clone.join("coordination/source-reader-closure-v1.json");
    let descriptor = fs::read(&descriptor_path).unwrap();
    match classify_b311_descriptor(&descriptor).unwrap() {
        B311DescriptorState::PostFlip => {
            let binding =
                fs::read_to_string(clone.join("coordination/PROJECT-BINDING.yaml")).unwrap();
            validate_post_flip_binding(&binding).unwrap();
            let events = fs::read(clone.join("coordination/rounds/r82/events.jsonl")).unwrap();
            validate_b311_recorded_owner_history(&events).unwrap();
            return clone;
        }
        B311DescriptorState::PreFlip => {}
    }

    let owner_merge = head(&clone);
    let events_path = clone.join("coordination/rounds/r82/events.jsonl");
    let existing_events = fs::read(&events_path).unwrap();
    let parsed_events = parse_event_lines(&existing_events).unwrap();
    assert!(parsed_events.iter().all(|event| {
        event.get("taskId").and_then(serde_json::Value::as_str) != Some("B311")
            || !matches!(
                event.get("type").and_then(serde_json::Value::as_str),
                Some("MergeExecuted" | "TaskRecorded")
            )
    }));
    let mut events = fs::OpenOptions::new()
        .append(true)
        .open(&events_path)
        .unwrap();
    writeln!(
        events,
        "{}",
        serde_json::json!({
            "eventId": "B311-READER-MERGE",
            "ts": "2026-08-28T00:00:00Z",
            "actor": "reviewer:orch-runtime",
            "type": "MergeExecuted",
            "taskId": "B311",
            "round": "r82",
            "payload": {"mergeSha": owner_merge, "policy": "no-ff"}
        })
    )
    .unwrap();
    writeln!(
        events,
        "{}",
        serde_json::json!({
            "eventId": "B311-READER-RECORDED",
            "ts": "2026-08-28T00:00:01Z",
            "actor": "runtime:orch",
            "type": "TaskRecorded",
            "taskId": "B311",
            "round": "r82",
            "payload": {"postMergeGates": "all-green"}
        })
    )
    .unwrap();
    drop(events);

    let descriptor = String::from_utf8(descriptor).unwrap();
    let needle = concat!(
        "    },\n",
        "    {\n",
        "      \"reader\": \"orch/crates/orch-cli/tests/handshake_cli.rs\""
    );
    let insertion = concat!(
        "    },\n",
        "    {\n",
        "      \"reader\": \"orch/crates/orch-cli/src/guide.rs\",\n",
        "      \"mechanism\": \"IncludeStr\",\n",
        "      \"subjects\": [\"orch/docs/AI-MECHANICAL-GUIDE.md\"],\n",
        "      \"runnableTarget\": {\"commandRef\": \"sourceReaderClosure\", \"package\": \"orch-cli\", \"test\": \"guide_cli\"}\n",
        "    },\n",
        "    {\n",
        "      \"reader\": \"orch/crates/orch-cli/tests/handshake_cli.rs\""
    );
    assert_eq!(descriptor.matches(needle).count(), 1);
    let descriptor = descriptor.replacen(needle, insertion, 1);
    let descriptor_sha = sha256_hex(descriptor.as_bytes());
    assert_eq!(descriptor_sha, B311_POST_FLIP_DESCRIPTOR_SHA256);
    fs::write(&descriptor_path, descriptor).unwrap();

    let binding_path = clone.join("coordination/PROJECT-BINDING.yaml");
    let binding = fs::read_to_string(&binding_path).unwrap();
    assert_eq!(binding.matches("ownerTask: B306").count(), 1);
    let binding = binding
        .replacen("ownerTask: B306", "ownerTask: B311", 1)
        .replacen(B311_PRE_FLIP_DESCRIPTOR_SHA256, &descriptor_sha, 1);
    fs::write(&binding_path, binding).unwrap();
    git_ok(&clone, &["add", "coordination/PROJECT-BINDING.yaml"]);
    git_ok(
        &clone,
        &["add", "coordination/source-reader-closure-v1.json"],
    );
    git_ok(&clone, &["add", "coordination/rounds/r82/events.jsonl"]);
    git_ok(
        &clone,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture: B311 production reader",
        ],
    );
    clone
}

fn dispatch_fixture(
    actor: &str,
    round: &str,
    task: &str,
    attempt: &str,
    attempt_no: u64,
    agent: &str,
    base_sha: &str,
) -> serde_json::Value {
    serde_json::json!({
        "eventId": "DISPATCH-FIXTURE",
        "ts": "2026-08-26T00:00:00Z",
        "actor": actor,
        "type": "DispatchIssued",
        "taskId": task,
        "round": round,
        "payload": {
            "attemptId": attempt,
            "attemptNo": attempt_no,
            "agent": agent,
            "baseSha": base_sha,
        }
    })
}

#[test]
fn b311_post_flip_fixture_reuses_committed_descriptor_and_owner_history() {
    let root = repo_root();
    let expected_head = legacy_source_reader_policy_base(&root);
    assert_eq!(
        classify_b311_descriptor(
            &orch_host::gitx::show_bytes(
                &root,
                &expected_head,
                "coordination/source-reader-closure-v1.json",
            )
            .unwrap(),
        )
        .unwrap(),
        B311DescriptorState::PostFlip
    );

    let clone = b311_reader_fixture("b311-post-flip-idempotent");
    assert_eq!(head(&clone), expected_head);
    validate_post_flip_binding(
        &fs::read_to_string(clone.join("coordination/PROJECT-BINDING.yaml")).unwrap(),
    )
    .unwrap();
    validate_b311_recorded_owner_history(
        &fs::read(clone.join("coordination/rounds/r82/events.jsonl")).unwrap(),
    )
    .unwrap();
    fs::remove_dir_all(clone).unwrap();
}

#[test]
fn b311_reader_fixture_rejects_an_unknown_descriptor_state() {
    let mut descriptor =
        fs::read(repo_root().join("coordination/source-reader-closure-v1.json")).unwrap();
    descriptor.extend_from_slice(b"\n");
    let error = classify_b311_descriptor(&descriptor).unwrap_err();
    assert!(error.contains("unknown digest"), "{error}");
}

#[test]
fn historical_policy_base_requires_one_canonical_dispatch() {
    let base = "a".repeat(40);
    let canonical = dispatch_fixture(
        "runtime:orch",
        "r81",
        "B304",
        "B304-A0001",
        1,
        "executor-desktop",
        &base,
    );
    assert_eq!(
        canonical_dispatch_base_from_events(
            std::slice::from_ref(&canonical),
            "r81",
            "B304",
            "B304-A0001",
            "executor-desktop",
        )
        .unwrap(),
        base
    );

    assert!(canonical_dispatch_base_from_events(
        &[],
        "r81",
        "B304",
        "B304-A0001",
        "executor-desktop",
    )
    .is_err());
    assert!(canonical_dispatch_base_from_events(
        &[canonical.clone(), canonical],
        "r81",
        "B304",
        "B304-A0001",
        "executor-desktop",
    )
    .is_err());

    for invalid in [
        dispatch_fixture(
            "planner",
            "r81",
            "B304",
            "B304-A0001",
            1,
            "executor-desktop",
            &"b".repeat(40),
        ),
        dispatch_fixture(
            "runtime:orch",
            "r81",
            "B304",
            "B304-A0002",
            1,
            "executor-desktop",
            &"b".repeat(40),
        ),
        dispatch_fixture(
            "runtime:orch",
            "r81",
            "B304",
            "B304-A0001",
            2,
            "executor-desktop",
            &"b".repeat(40),
        ),
        dispatch_fixture(
            "runtime:orch",
            "r81",
            "B304",
            "B304-A0001",
            1,
            "executor-pi",
            &"b".repeat(40),
        ),
        dispatch_fixture(
            "runtime:orch",
            "r81",
            "B304",
            "B304-A0001",
            1,
            "executor-desktop",
            &"B".repeat(40),
        ),
    ] {
        assert!(canonical_dispatch_base_from_events(
            &[invalid],
            "r81",
            "B304",
            "B304-A0001",
            "executor-desktop",
        )
        .is_err());
    }
}

#[test]
fn attempt_external_baseline_edge_resolves_to_its_exact_runtime_test() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    for subject in [
        "orch/crates/orch-host/src/attempt.rs",
        "orch/crates/orch-host/src/attempt/tests_body.rs",
    ] {
        let decision = resolve_source_reader_closure_v1(
            &root,
            &root,
            &policy_base,
            &[subject.to_string()],
        )
        .unwrap();
        let SourceReaderClosureDecisionV1::Closed {
            targets,
            descriptor_sha256,
            base_sha256,
        } = decision
        else {
            panic!("signed overlay should close for {subject}: {decision:?}");
        };
        assert_eq!(descriptor_sha256, B311_POST_FLIP_DESCRIPTOR_SHA256);
        assert_eq!(
            base_sha256,
            "80249192a0019bf65f6d59e9c0a828a3e74c6ee37fcb347b13ce5fade533986d"
        );
        assert!(targets.iter().any(|target| {
            target.command_ref == "seedTargets"
                && target.package == "orch-host"
                && target.test == "attempt_body_relocation"
                && target.reader == "orch/crates/orch-host/tests/attempt_body_relocation.rs"
        }));
    }
}

#[test]
fn newly_landed_b310_reader_forces_b304_verify_to_fast() {
    let root = repo_root();
    let policy_base =
        recorded_dispatch_base(&root, "r81", "B304", "B304-A0001", "executor-desktop").unwrap();
    let decision = resolve_source_reader_closure_at_tree_v1(
        &root,
        "r81",
        &policy_base,
        &policy_base,
        &["orch/crates/orch-host/src/verify.rs".to_string()],
    )
    .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("the post-descriptor B310 reader must never authorize a narrow B304 collect");
    };
    assert!(reason.contains("983adb428653f908842e31dbffcefda7b50ec3bd888eff205607c37c6f9bf512"));
    assert!(reason.contains("review_panel_runtime_contract.rs"));
    assert!(reason.contains("orch/crates/orch-host/src/verify.rs"));
}

#[test]
fn r81_runtime_escalation_list_is_exact_and_mandatory() {
    let root = repo_root();
    let clone = scratch("tampered-runtime-escalations");
    let output = Command::new("git")
        .args(["clone", "--quiet", "--shared"])
        .arg(&root)
        .arg(&clone)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    switch_to_legacy_source_reader_policy_base(&root, &clone);
    let manifest_path =
        clone.join("coordination/rounds/r81/planning/source-reader-runtime-escalations.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["edges"]
        .as_array_mut()
        .unwrap()
        .retain(|edge| edge["subject"] != "orch/crates/orch-host/src/verify.rs");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    for args in [
        vec![
            "add",
            "coordination/rounds/r81/planning/source-reader-runtime-escalations.json",
        ],
        vec![
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-m",
            "tamper escalation list",
        ],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let tampered = rev(&clone, "HEAD");
    let decision = resolve_source_reader_closure_at_tree_v1(
        &clone,
        "r81",
        &tampered,
        &tampered,
        &["orch/crates/orch-host/src/verify.rs".to_string()],
    )
    .unwrap();
    assert!(decision
        .upgrade_reason()
        .is_some_and(|reason| reason.contains("SHA 漂移")));

    fs::remove_file(&manifest_path).unwrap();
    let output = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args([
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-am",
            "remove escalation list",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let missing = rev(&clone, "HEAD");
    let decision = resolve_source_reader_closure_at_tree_v1(
        &clone,
        "r81",
        &missing,
        &missing,
        &["orch/crates/orch-host/src/verify.rs".to_string()],
    )
    .unwrap();
    assert!(decision
        .upgrade_reason()
        .is_some_and(|reason| reason.contains("清单缺失")));
    fs::remove_dir_all(clone).unwrap();
}

#[test]
fn an_unregistered_dynamic_reader_upgrades_instead_of_running_only_check() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    let candidate = scratch("unknown");
    let reader = "orch/crates/orch-host/tests/b306_unknown_reader.rs";
    let path = candidate.join(reader);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "macro_rules! hidden { () => { include_str!(concat!(\"../src/\", NAME)) } }\n",
    )
    .unwrap();

    let decision =
        resolve_source_reader_closure_v1(&root, &candidate, &policy_base, &[reader.to_string()])
            .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("unknown reader must upgrade");
    };
    assert!(reason.contains("unregistered source reader"));
    fs::remove_dir_all(candidate).unwrap();
}

#[test]
fn whitespace_between_reader_and_parenthesis_still_upgrades() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    let candidate = scratch("whitespace-reader");
    let reader = "orch/crates/orch-host/tests/b306_whitespace_reader.rs";
    let path = candidate.join(reader);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "const SUBJECT: &str = include_str! (\"../src/gate.rs\");\n",
    )
    .unwrap();

    let decision =
        resolve_source_reader_closure_v1(&root, &candidate, &policy_base, &[reader.to_string()])
            .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("whitespace must not hide an unregistered reader");
    };
    assert!(reason.contains("unregistered source reader"));
    fs::remove_dir_all(candidate).unwrap();
}

#[test]
fn token_trivia_cannot_hide_unregistered_macro_or_fs_readers() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    for (tag, source) in [
        (
            "block-comment-macro",
            "const SUBJECT: &str = include_str /* reader trivia */ ! (\"../src/gate.rs\");\n",
        ),
        (
            "line-comment-macro",
            "const SUBJECT: &[u8] = include_bytes // reader trivia\n! (\"../src/gate.rs\");\n",
        ),
        (
            "spaced-fs-path",
            "#[test]\nfn reads_subject() { let _ = std :: fs :: read(\"../src/gate.rs\"); }\n",
        ),
    ] {
        let candidate = scratch(tag);
        let reader = format!("orch/crates/orch-host/tests/b306_{tag}.rs");
        let path = candidate.join(&reader);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();

        let decision = resolve_source_reader_closure_v1(
            &root,
            &candidate,
            &policy_base,
            std::slice::from_ref(&reader),
        )
        .unwrap();
        let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
            panic!("token trivia hid an unregistered reader in {reader}");
        };
        assert!(reason.contains("unregistered source reader"), "{reason}");
        fs::remove_dir_all(candidate).unwrap();
    }
}

#[test]
fn fs_aliases_and_open_options_cannot_hide_unregistered_readers() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    for (tag, source) in [
        (
            "aliased-fs-read",
            "use std::fs as disk;\n#[test]\nfn reads() { let _ = disk::read(\"../src/gate.rs\"); }\n",
        ),
        (
            "open-options",
            "use std::fs::OpenOptions;\n#[test]\nfn reads() { let _ = OpenOptions::new().read(true).open(\"../src/gate.rs\"); }\n",
        ),
    ] {
        let candidate = scratch(tag);
        let reader = format!("orch/crates/orch-host/tests/b306_{tag}.rs");
        let path = candidate.join(&reader);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();

        let decision = resolve_source_reader_closure_v1(
            &root,
            &candidate,
            &policy_base,
            std::slice::from_ref(&reader),
        )
        .unwrap();
        let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
            panic!("an aliased reader escaped classification in {reader}");
        };
        assert!(reason.contains("unregistered source reader"), "{reason}");
        fs::remove_dir_all(candidate).unwrap();
    }
}

#[test]
fn changing_a_registered_literal_reader_requires_fast_reclassification() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    let candidate = scratch("changed-registered");
    let reader = "orch/crates/orch-host/tests/gate_observation_identity.rs";
    let path = candidate.join(reader);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "#[test]\nfn removed_the_registered_reader_edge() {}\n",
    )
    .unwrap();

    let decision =
        resolve_source_reader_closure_v1(&root, &candidate, &policy_base, &[reader.to_string()])
            .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("changed registered reader must upgrade");
    };
    assert!(reason.contains("registered reader bytes changed"));
    fs::remove_dir_all(candidate).unwrap();
}

#[test]
fn a_unit_test_reader_without_an_integration_target_upgrades() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    let candidate = scratch("unit-reader");
    let reader = "orch/crates/orch-host/src/b306_unit_reader.rs";
    let path = candidate.join(reader);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        "#[cfg(test)]\nmod tests { const SUBJECT: &str = include_str!(\"gate.rs\"); }\n",
    )
    .unwrap();

    let decision =
        resolve_source_reader_closure_v1(&root, &candidate, &policy_base, &[reader.to_string()])
            .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("unit-test reader must upgrade");
    };
    assert!(reason.contains("cannot convert to an integration target"));
    fs::remove_dir_all(candidate).unwrap();
}

#[test]
fn cfg_token_trivia_and_compound_test_cfg_cannot_hide_unit_readers() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    for (tag, attribute) in [
        ("spaced-cfg", "#[cfg ( test )]"),
        ("compound-cfg", "#[cfg(all(test, unix))]"),
    ] {
        let candidate = scratch(tag);
        let reader = format!("orch/crates/orch-host/src/b306_{tag}.rs");
        let path = candidate.join(&reader);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!(
                "{attribute}\nmod tests {{ const SUBJECT: &str = include_str /* trivia */ ! (\"gate.rs\"); }}\n"
            ),
        )
        .unwrap();

        let decision = resolve_source_reader_closure_v1(
            &root,
            &candidate,
            &policy_base,
            std::slice::from_ref(&reader),
        )
        .unwrap();
        let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
            panic!("cfg token trivia hid a unit reader in {reader}");
        };
        assert!(
            reason.contains("cannot convert to an integration target"),
            "{reason}"
        );
        fs::remove_dir_all(candidate).unwrap();
    }
}

#[test]
fn a_plain_test_attribute_cannot_hide_an_unknown_literal_source_reader() {
    let root = repo_root();
    let policy_base = legacy_source_reader_policy_base(&root);
    for (tag, reader, source) in [(
        "plain-test-reader",
        "orch/crates/orch-host/src/b306_plain_test_reader.rs",
        "#[test]\nfn reads() { let _ = std::fs::read(\"../src/gate.rs\"); }\n",
    )] {
        let candidate = scratch(tag);
        let path = candidate.join(reader);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();

        let decision = resolve_source_reader_closure_v1(
            &root,
            &candidate,
            &policy_base,
            &[reader.to_string()],
        )
        .unwrap();
        let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
            panic!("unknown reader syntax escaped classification in {reader}");
        };
        assert!(reason.contains("reader"), "{reason}");
        fs::remove_dir_all(candidate).unwrap();
    }
}

#[test]
fn a_policy_base_literal_reader_omitted_from_the_descriptor_upgrades_on_subject_change() {
    let root = repo_root();
    let clone = scratch("policy-base-unknown-reader");
    let output = Command::new("git")
        .args(["clone", "--quiet", "--shared"])
        .arg(&root)
        .arg(&clone)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    switch_to_legacy_source_reader_policy_base(&root, &clone);

    let reader = "orch/crates/orch-host/tests/b306_policy_base_reader.rs";
    let subject = "orch/crates/orch-host/src/b306_policy_base_subject.rs";
    fs::write(
        clone.join(reader),
        "const SUBJECT: &str = include_str!(\"../src/b306_policy_base_subject.rs\");\n",
    )
    .unwrap();
    fs::write(clone.join(subject), "pub const VALUE: u8 = 1;\n").unwrap();
    let output = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args([
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "add",
            reader,
            subject,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args([
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-m",
            "policy base unknown reader",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let policy_base = rev(&clone, "HEAD");

    fs::write(clone.join(subject), "pub const VALUE: u8 = 2;\n").unwrap();
    let output = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args([
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-am",
            "change reader subject",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let candidate = rev(&clone, "HEAD");

    let decision = resolve_source_reader_closure_at_tree_v1(
        &clone,
        "r82",
        &candidate,
        &policy_base,
        &[subject.to_string()],
    )
    .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("policy-base reader omitted from the descriptor escaped the closure");
    };
    assert!(reason.contains(reader), "{reason}");
    assert!(reason.contains(subject), "{reason}");
    fs::remove_dir_all(clone).unwrap();
}

#[test]
fn b311_historical_guide_closure_does_not_silence_a_real_unknown_edge() {
    let clone = b311_reader_fixture("b311-guide-positive");
    let policy_base = head(&clone);
    let decision = resolve_source_reader_closure_at_tree_v1(
        &clone,
        "r82",
        &policy_base,
        &policy_base,
        &["orch/docs/AI-MECHANICAL-GUIDE.md".to_owned()],
    )
    .unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("historical unknown edge must not gain narrow reuse authority");
    };
    assert!(reason.contains("channel_closure_and_reader_bootstrap.rs"), "{reason}");
    assert!(reason.contains("orch/docs/AI-MECHANICAL-GUIDE.md"), "{reason}");
    fs::remove_dir_all(clone).unwrap();
}

#[test]
fn b311_wrong_owner_or_unrunnable_production_target_upgrades_to_fast() {
    let wrong_owner = b311_reader_fixture("b311-wrong-owner");
    let binding_path = wrong_owner.join("coordination/PROJECT-BINDING.yaml");
    let binding = fs::read_to_string(&binding_path).unwrap();
    let owner = "candidate-lanes-v1:\n      schemaVersion: 1\n      ownerTask: B311";
    let wrong = "candidate-lanes-v1:\n      schemaVersion: 1\n      ownerTask: B999";
    assert_eq!(binding.matches(owner).count(), 1);
    let binding = binding.replacen(owner, wrong, 1);
    fs::write(&binding_path, binding).unwrap();
    git_ok(&wrong_owner, &["add", "coordination/PROJECT-BINDING.yaml"]);
    git_ok(
        &wrong_owner,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-m",
            "mutate owner",
        ],
    );
    let owner_head = head(&wrong_owner);
    let error = authenticated_reader_target_for_test(
        &wrong_owner,
        &owner_head,
        "orch/crates/orch-cli/src/guide.rs",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("candidate policy owner"),
        "{error:#}"
    );
    fs::remove_dir_all(wrong_owner).unwrap();

    let wrong_target = b311_reader_fixture("b311-wrong-target");
    let descriptor_path = wrong_target.join("coordination/source-reader-closure-v1.json");
    let descriptor = fs::read_to_string(&descriptor_path).unwrap().replacen(
        "\"test\": \"guide_cli\"",
        "\"test\": \"missing_guide_cli\"",
        1,
    );
    let digest = hex::encode(Sha256::digest(descriptor.as_bytes()));
    fs::write(&descriptor_path, descriptor).unwrap();
    let binding_path = wrong_target.join("coordination/PROJECT-BINDING.yaml");
    let binding = fs::read_to_string(&binding_path).unwrap().replacen(
        "8f8a4fc69261c42b5831cd7d3fe711034c9779f6184cd3875d7a2bd9eaa1a005",
        &digest,
        1,
    );
    fs::write(&binding_path, binding).unwrap();
    git_ok(&wrong_target, &["add", "coordination/PROJECT-BINDING.yaml"]);
    git_ok(
        &wrong_target,
        &["add", "coordination/source-reader-closure-v1.json"],
    );
    git_ok(
        &wrong_target,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-m",
            "mutate production target",
        ],
    );
    let target_head = head(&wrong_target);
    let error = authenticated_reader_target_for_test(
        &wrong_target,
        &target_head,
        "orch/crates/orch-cli/src/guide.rs",
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("runnable target 不存在"),
        "{error:#}"
    );
    fs::remove_dir_all(wrong_target).unwrap();
}

#[test]
fn b311_production_reader_byte_drift_still_upgrades_to_fast() {
    let clone = b311_reader_fixture("b311-guide-drift");
    let policy_base = head(&clone);
    let reader = "orch/crates/orch-cli/src/guide.rs";
    let mut bytes = fs::read(clone.join(reader)).unwrap();
    bytes.extend_from_slice(b"\n// B311 drift fixture\n");
    fs::write(clone.join(reader), bytes).unwrap();
    git_ok(&clone, &["add", reader]);
    git_ok(
        &clone,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "--quiet",
            "-m",
            "drift production reader",
        ],
    );
    let candidate = head(&clone);
    let decision = resolve_source_reader_closure_at_tree_v1(
        &clone,
        "r82",
        &candidate,
        &policy_base,
        &[reader.to_string()],
    )
    .unwrap();
    assert!(
        decision
            .upgrade_reason()
            .is_some_and(|reason| reason.contains("registered reader bytes changed")),
        "{decision:?}"
    );
    fs::remove_dir_all(clone).unwrap();
}

#[test]
fn all_four_existing_wake_source_reader_markers_remain_registered() {
    let descriptor =
        fs::read_to_string(repo_root().join("coordination/source-reader-closure-v1.json")).unwrap();
    for reader in [
        "harness_capability_admission.rs",
        "harness_invocation_envelope.rs",
        "harness_terminal_envelope.rs",
        "reassignment_recovery_contract.rs",
    ] {
        assert!(
            descriptor.contains(reader),
            "existing reader marker lost: {reader}"
        );
    }
}

#[test]
fn b328_historical_descriptor_byte_drift_does_not_gain_reuse_authority() {
    let clone = b311_reader_fixture("descriptor-byte-drift");
    let path = clone.join("coordination/source-reader-closure-v1.json");
    let mut bytes = fs::read(&path).unwrap();
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap();
    git_ok(
        &clone,
        &["add", "coordination/source-reader-closure-v1.json"],
    );
    git_ok(
        &clone,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            "unbound descriptor bytes",
        ],
    );
    let fixed = head(&clone);
    let decision =
        resolve_source_reader_closure_at_tree_v1(&clone, "r82", &fixed, &fixed, &[]).unwrap();
    let SourceReaderClosureDecisionV1::UpgradeToFast { reason, .. } = decision else {
        panic!("unbound descriptor gained reuse authority")
    };
    assert!(
        reason.contains("source-reader descriptor SHA 漂移"),
        "{reason}"
    );
    fs::remove_dir_all(clone).unwrap();
}
