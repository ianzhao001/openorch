//! 红种子契约 · B106 · Rust 门必须从第一次执行起 locked。
//! 预期红：compile，缺少 validate_locked_rust_gates。
//! M1：testFast 缺 --locked 仍通过，missing_locked_is_rejected 红。
//! M2：只检查 check 不检查 testFast，同一用例红。
//! M3：把 `--` 后的 --locked 当 Cargo 选项，locked_after_terminator_is_rejected 红。

use orch_host::binding::{validate_locked_rust_gates, Binding};

fn binding(test_argv: &str, check_argv: &str) -> Binding {
    serde_yaml::from_str(&format!(
        "project: {{ecosystems: [rust]}}\ncommands:\n  testFast:\n    argv: [{test_argv}]\n  check:\n    argv: [{check_argv}]\n"
    ))
    .unwrap()
}

#[test]
fn both_locked_commands_are_accepted() {
    let b = binding(
        "cargo, test, --workspace, --locked",
        "cargo, check, --workspace, --locked",
    );
    assert!(validate_locked_rust_gates(&b).is_ok());
}

#[test]
fn missing_locked_is_rejected() {
    let b = binding("cargo, test, --workspace", "cargo, check, --workspace");
    let errors = validate_locked_rust_gates(&b).unwrap_err();
    assert!(errors.iter().any(|e| e.contains("testFast")), "{errors:?}");
    assert!(errors.iter().any(|e| e.contains("check")), "{errors:?}");
}

#[test]
fn locked_after_terminator_is_rejected() {
    let b = binding(
        "cargo, test, --workspace, --, --locked",
        "cargo, check, --workspace, --locked",
    );
    assert!(validate_locked_rust_gates(&b).is_err());
}
