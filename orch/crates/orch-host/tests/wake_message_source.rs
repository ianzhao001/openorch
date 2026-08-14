//! 红种子契约 · B109 · wake/nudge 富输入来源。
//! 预期红：compile，缺少 MessageInput/MessageSource/resolve_message_input。
//! M1：default 不显式标识来源，default_source_is_visible 红。
//! M2：file 与 explicit 同时给出仍通过，sources_are_mutually_exclusive 红。
//! M3：按 char 数而非 UTF-8 bytes 计数，utf8_uses_byte_length 红。

use orch_host::wake::{resolve_message_input, MessageInput, MessageSource};

#[test]
fn default_source_is_visible() {
    let resolved = resolve_message_input(MessageInput::default(), "fallback").unwrap();
    assert_eq!(resolved.text, "fallback");
    assert_eq!(resolved.source, MessageSource::Default);
    assert_eq!(resolved.bytes, 8);
}

#[test]
fn sources_are_mutually_exclusive() {
    let input = MessageInput {
        explicit: Some("inline".into()),
        file_contents: Some("file".into()),
        stdin_contents: None,
    };
    assert!(resolve_message_input(input, "fallback").is_err());
}

#[test]
fn utf8_uses_byte_length() {
    let input = MessageInput {
        explicit: Some("中文".into()),
        file_contents: None,
        stdin_contents: None,
    };
    let resolved = resolve_message_input(input, "fallback").unwrap();
    assert_eq!(resolved.source, MessageSource::Explicit);
    assert_eq!(resolved.bytes, "中文".as_bytes().len());
}
