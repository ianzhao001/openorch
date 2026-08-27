use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use orch_host::srcshape::{
    resolve_source_reader_closure_at_tree_v1, resolve_source_reader_closure_v1,
    SourceReaderClosureDecisionV1,
};

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

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

#[test]
fn attempt_external_baseline_edge_resolves_to_its_exact_runtime_test() {
    let root = repo_root();
    for subject in [
        "orch/crates/orch-host/src/attempt.rs",
        "orch/crates/orch-host/src/attempt/tests_body.rs",
    ] {
        let decision =
            resolve_source_reader_closure_v1(&root, &root, &head(&root), &[subject.to_string()])
                .unwrap();
        let SourceReaderClosureDecisionV1::Closed {
            targets,
            descriptor_sha256,
            base_sha256,
        } = decision
        else {
            panic!("signed overlay should close for {subject}: {decision:?}");
        };
        assert_eq!(
            descriptor_sha256,
            "d003a17e81b783952d385c2cd6d082480c84108fff0b8cfff1c851373c67e489"
        );
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
    let policy_base = rev(&root, "main");
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
        resolve_source_reader_closure_v1(&root, &candidate, &head(&root), &[reader.to_string()])
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
        resolve_source_reader_closure_v1(&root, &candidate, &head(&root), &[reader.to_string()])
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
            &head(&root),
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
            &head(&root),
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
        resolve_source_reader_closure_v1(&root, &candidate, &head(&root), &[reader.to_string()])
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
        resolve_source_reader_closure_v1(&root, &candidate, &head(&root), &[reader.to_string()])
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
            &head(&root),
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
            &head(&root),
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
