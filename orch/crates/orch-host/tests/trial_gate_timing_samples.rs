//! B203 · H56/H75 门内多采样 + H78 假 multica 读截止合同。
//!
//! 首红：compile E0432，`support_multica::retry_transient_read_until_with_clock` 尚不存在。
//! 本卡不得通过修改 collect.rs 重跑整套 trial gate；多采样必须位于两个既有时序测试内部。
//!
//! M1. WouldBlock/Interrupted/TimedOut 任一不重试；
//! M2. 每轮重建 deadline；M3. fatal error 也重试；
//! M4. serve 或 wake 的样本数降到 1；M5. socket 回到仓内或 Drop 不清理。

#![cfg(unix)]

mod support_multica;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use support_multica::{
    retry_transient_read_until_with_clock, FakeMultica, MulticaScript,
};

#[test]
fn would_block_interrupted_and_timed_out_retry_before_success() {
    let start = Instant::now();
    let script = RefCell::new(VecDeque::from([
        Err(io::Error::from(io::ErrorKind::WouldBlock)),
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Err(io::Error::from(io::ErrorKind::TimedOut)),
        Ok("request-line"),
    ]));
    let calls = Cell::new(0usize);
    let value = retry_transient_read_until_with_clock(
        start + Duration::from_secs(30),
        || start,
        |_remaining| {
            calls.set(calls.get() + 1);
            script.borrow_mut().pop_front().unwrap()
        },
    )
    .unwrap();
    assert_eq!(value, "request-line");
    assert_eq!(calls.get(), 4);
}

#[test]
fn transient_retries_stop_at_the_original_deadline() {
    let start = Instant::now();
    let times = RefCell::new(VecDeque::from([
        start,
        start + Duration::from_secs(1),
        start + Duration::from_secs(3),
    ]));
    let calls = Cell::new(0usize);
    let budgets = RefCell::new(Vec::new());
    let error = retry_transient_read_until_with_clock::<(), _, _>(
        start + Duration::from_secs(2),
        || times.borrow_mut().pop_front().unwrap(),
        |remaining| {
            calls.set(calls.get() + 1);
            budgets.borrow_mut().push(remaining);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(calls.get(), 2, "deadline crossing must prevent a third read");
    let budgets = budgets.into_inner();
    assert!(budgets[1] < budgets[0], "remaining budget must be monotonic");
}

#[test]
fn fatal_read_error_is_not_retried() {
    let start = Instant::now();
    let calls = Cell::new(0usize);
    let error = retry_transient_read_until_with_clock::<(), _, _>(
        start + Duration::from_secs(30),
        || start,
        |_remaining| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(io::Error::from(io::ErrorKind::InvalidData))
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(calls.get(), 1);
}

fn assert_ten_sample_wrapper(source: &str, test_name: &str) {
    assert!(source.contains(test_name), "missing test {test_name}");
    assert!(
        source.contains("const B203_TRIAL_GATE_SAMPLES: usize = 10;"),
        "{test_name} must declare the r62 sampling floor"
    );
    assert!(
        source.contains("for sample in 0..B203_TRIAL_GATE_SAMPLES"),
        "{test_name} must execute every sample inside one testFast run"
    );
    assert!(
        source.contains("_one_sample(sample)"),
        "{test_name} must delegate unchanged behavior to a one-sample helper"
    );
}

#[test]
fn h56_and_h75_each_have_ten_in_gate_samples() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let serve = std::fs::read_to_string(manifest.join("src/serve.rs")).unwrap();
    let wake = std::fs::read_to_string(manifest.join("src/wake.rs")).unwrap();
    assert_ten_sample_wrapper(
        &serve,
        "production_entries_visit_all_six_unattended_stations",
    );
    assert_ten_sample_wrapper(
        &wake,
        "b177_same_pgid_child_prevents_false_natural_success_after_leader_exit",
    );
}

#[test]
fn h69_socket_endpoint_is_short_external_and_removed() {
    let fake = FakeMultica::spawn("b203-path", MulticaScript::ZeroFrameEof);
    let path = fake.sock_path().to_path_buf();
    let bytes = path.as_os_str().as_bytes().len();
    let temp = std::env::temp_dir();
    assert!(
        path.starts_with(&temp) || path.starts_with("/tmp"),
        "socket must be outside the trial clone: {}",
        path.display()
    );
    assert!(bytes < 104, "bytes={bytes} limit=104 path={}", path.display());
    drop(fake);
    assert!(!path.exists(), "Drop must remove socket: {}", path.display());
}
