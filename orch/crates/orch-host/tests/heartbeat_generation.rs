//! B112 seed · cross-restart attempt clock and heartbeat identity.
//!
//! 预期红：compile，缺少 attempt_elapsed_from_dispatch / HeartbeatIdentity / heartbeat_is_current。
//! M1：elapsed 从当前 daemon 启动计时而非 DispatchIssued.ts → restart_does_not_reset_attempt_age 红。
//! M2：忽略 round → old_round_heartbeat_is_stale 红。
//! M3：忽略 generation → old_generation_heartbeat_is_stale 红。

use orch_host::liveness::{
    attempt_elapsed_from_dispatch, heartbeat_is_current, HeartbeatIdentity,
};

#[test]
fn restart_does_not_reset_attempt_age() {
    let elapsed = attempt_elapsed_from_dispatch(
        "2026-07-26T00:00:00Z",
        "2026-07-26T00:05:30Z",
    )
    .unwrap();
    assert_eq!(elapsed.as_secs(), 330);
}

#[test]
fn old_round_heartbeat_is_stale() {
    let hb = HeartbeatIdentity::new("r46", "B112-A0001", 99).unwrap();
    assert!(!heartbeat_is_current("r47", "B112-A0001", &hb));
}

#[test]
fn old_generation_heartbeat_is_stale() {
    let hb = HeartbeatIdentity::new("r47", "B112-A0001", 99).unwrap();
    assert!(!heartbeat_is_current("r47", "B112-A0002", &hb));
    assert!(heartbeat_is_current("r47", "B112-A0001", &hb));
}
