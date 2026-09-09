//! B219 · H80 workspace 基线时序合同。
//!
//! 首红：compile E0583，`h80_test_support` 尚不存在。该最小桥只负责让冻结 seed
//! 进入行为断言；当前 test-only typed-receipt 等待仅 1 秒，且 B191 在 stubborn provider
//! 安装 TERM handler 之前就发 cancel，两项行为断言随后必须同时验红。
//!
//! M1. test-only receipt timeout 退回 1 秒；M2. cancel 前删除 provider-ready 等待；
//! M3. ready 等待退化为固定 sleep；M4. production 300 秒预算被一起改动。

#![cfg(unix)]

use std::fs;
use std::path::Path;

mod h80_test_support;

fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source
        .find(start)
        .unwrap_or_else(|| panic!("missing start marker: {start}"));
    let tail = &source[start..];
    let end = tail
        .find(end)
        .unwrap_or_else(|| panic!("missing end marker after {start}: {end}"));
    &tail[..end]
}

#[test]
fn test_receipt_timeout_has_load_headroom_without_touching_production() {
    h80_test_support::contract_loaded();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let wake = fs::read_to_string(manifest.join("src/wake.rs")).unwrap();
    let timeout = between(
        &wake,
        "pub fn review_probe_timeout()",
        "/// Contract probe used by the immutable seeded test.",
    );

    assert!(
        timeout.contains("#[cfg(test)]") && timeout.contains("Duration::from_secs(5)"),
        "test-only typed-receipt budget must be 5s so a loaded full gate does not reject a healthy fixture"
    );
    assert!(
        timeout.contains("#[cfg(not(test))]") && timeout.contains("Duration::from_secs(300)"),
        "production review/receipt budget must remain exactly 300s"
    );
}

#[test]
fn stubborn_provider_is_proven_ready_before_cancel_and_kill_contract_stays_strong() {
    h80_test_support::contract_loaded();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let wake = fs::read_to_string(manifest.join("src/wake.rs")).unwrap();
    let helper = between(
        &wake,
        "fn wait_for_stubborn_provider_terminal(",
        "fn b191_real_managed_supervisor_cancel_uses_the_existing_cleanup_owner()",
    );
    assert!(
        helper.contains("B177_FIXTURE_TERMINAL_RECORD")
            && helper.contains("Instant::now()")
            && helper.contains("deadline"),
        "provider-ready proof must observe the exact post-handler terminal record under one absolute deadline"
    );

    let test = between(
        &wake,
        "fn b191_real_managed_supervisor_cancel_uses_the_existing_cleanup_owner()",
        "fn b191_real_hidden_supervisor_reports_review_hard_deadline()",
    );
    let ready = test
        .find("wait_for_stubborn_provider_terminal(")
        .expect("B191 must wait for stubborn-provider readiness");
    let cancel = test
        .find("request_managed_wake_cancel(")
        .expect("B191 must still exercise the real cancel path");
    assert!(ready < cancel, "readiness proof must happen before cancel");
    assert!(
        test.contains("assert_eq!(status.signals, vec![\"TERM\", \"KILL\"]);"),
        "the fix must synchronize the fixture, not weaken the TERM→KILL assertion"
    );
}
