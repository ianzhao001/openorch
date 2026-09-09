use std::path::{Path, PathBuf};

use orch_host::wake::validate_smartclaw_payload_terminal_for_test;
use sha2::{Digest, Sha256};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("orch-host manifest must be three levels below the repository root")
        .to_path_buf()
}

#[test]
fn the_real_schema_sample_binds_the_mandatory_nested_session_and_optional_top_level() {
    let bytes = std::fs::read(
        repo_root().join("coordination/rounds/r82/planning/smartclaw-terminal-schema-sample.jsonl"),
    )
    .unwrap();
    assert_eq!(bytes.len(), 365);
    assert_eq!(
        hex::encode(Sha256::digest(&bytes)),
        "3649a19c4c241ca5df1889096d9e775602fc057234367d7339dfba91c62cdc6f"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("sessionId").is_none());
    let session = "orch-wake-r82-terminal-schema-a4f7";
    assert_eq!(
        value
            .pointer("/meta/agentMeta/sessionId")
            .and_then(|v| v.as_str()),
        Some(session)
    );
    let text = value["payloads"][0]["text"].as_str().unwrap();
    let terminal = validate_smartclaw_payload_terminal_for_test(&bytes, session)
        .unwrap()
        .expect("the LF-complete sample is terminal");
    assert_eq!(terminal.0, session);
    assert_eq!(terminal.1, hex::encode(Sha256::digest(text.as_bytes())));
}

fn terminal(session: &str) -> Vec<u8> {
    format!(
        "{{\"payloads\":[{{\"text\":\"done\"}}],\"meta\":{{\"agentMeta\":{{\"sessionId\":\"{session}\"}}}}}}\n"
    )
    .into_bytes()
}

#[test]
fn nested_partial_and_duplicate_payload_objects_are_never_terminal() {
    let session = "orch-wake-11111111-2222-4333-8444-555555555555";
    let nested = format!(
        "{{\"wrapper\":{{\"payloads\":[{{\"text\":\"done\"}}]}},\"meta\":{{\"agentMeta\":{{\"sessionId\":\"{session}\"}}}}}}\n"
    );
    assert!(
        validate_smartclaw_payload_terminal_for_test(nested.as_bytes(), session)
            .unwrap()
            .is_none()
    );

    let mut partial = terminal(session);
    assert_eq!(partial.pop(), Some(b'\n'));
    assert!(
        validate_smartclaw_payload_terminal_for_test(&partial, session)
            .unwrap()
            .is_none()
    );

    let mut duplicate = terminal(session);
    duplicate.extend(terminal(session));
    assert!(validate_smartclaw_payload_terminal_for_test(&duplicate, session).is_err());
}

#[test]
fn every_one_of_the_four_session_value_failures_is_closed() {
    let expected = "orch-wake-aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
    assert!(
        validate_smartclaw_payload_terminal_for_test(&terminal("orch-wake-wrong"), expected,)
            .is_err()
    );

    for malformed in [
        b"{\"payloads\":[{\"text\":\"done\"}],\"meta\":{}}\n".as_slice(),
        b"{\"payloads\":[{\"text\":\"done\"}],\"meta\":{\"agentMeta\":{}}}\n".as_slice(),
        b"{\"payloads\":[{\"text\":\"done\"}]}\n".as_slice(),
    ] {
        assert!(validate_smartclaw_payload_terminal_for_test(malformed, expected).is_err());
    }

    let top_level_wrong = format!(
        "{{\"payloads\":[{{\"text\":\"done\"}}],\"sessionId\":\"wrong\",\"meta\":{{\"agentMeta\":{{\"sessionId\":\"{expected}\"}}}}}}\n"
    );
    assert!(
        validate_smartclaw_payload_terminal_for_test(top_level_wrong.as_bytes(), expected,)
            .is_err()
    );
}

#[test]
fn production_parser_is_driver_truth_without_panel_magic() {
    let source =
        std::fs::read_to_string(repo_root().join("orch/crates/orch-host/src/wake.rs")).unwrap();
    assert!(source.contains("fn parse_smartclaw_payload_terminal_v1"));
    assert!(source.contains("trusted_smartclaw_channel_answer"));
    assert!(!source.contains("smartclaw_session_terminal_from_payloads_v1"));
    assert!(!source.contains("smartclaw_panel_artifact_v1"));
    assert!(!source.contains("SmartClawTerminalRouteDispositionV1"));
}
