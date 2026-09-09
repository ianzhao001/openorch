//! B110 seed · action-scoped channel facts.
//!
//! 预期红：compile，缺少 ActionChannelFacts / DeliveryState / exact_probe_slice。
//! M1：exact_probe_slice 忽略 start/end、退化为整份日志 → exact_window_excludes_old_noise 红。
//! M2：spawn 成功但尚未 engaged 时仍返回 Pending → delivered_only_means_spawn_succeeded 红。
//! M3：允许空 actionId/attemptId 或 attemptNo=0 → identity_is_fail_closed 红。

use orch_host::wake::{exact_probe_slice, ActionChannelFacts, DeliveryState};

#[test]
fn exact_window_excludes_old_noise() {
    let log = b"old-fault\nturn.started\nnew-tail\n";
    let start = b"old-fault\n".len() as u64;
    let end = (b"old-fault\nturn.started\n".len()) as u64;
    assert_eq!(exact_probe_slice(log, start, end).unwrap(), b"turn.started\n");
}

#[test]
fn delivered_only_means_spawn_succeeded() {
    assert_eq!(DeliveryState::from_spawn(false), DeliveryState::Pending);
    assert_eq!(DeliveryState::from_spawn(true), DeliveryState::Delivered);
}

#[test]
fn identity_is_fail_closed() {
    assert!(ActionChannelFacts::new("", "B110-A0001", 1, 7, "log", 0, 12).is_err());
    assert!(ActionChannelFacts::new("act", "", 1, 7, "log", 0, 12).is_err());
    assert!(ActionChannelFacts::new("act", "B110-A0001", 0, 7, "log", 0, 12).is_err());
    let facts = ActionChannelFacts::new("act", "B110-A0001", 1, 7, "log", 3, 12).unwrap();
    assert_eq!(facts.action_id, "act");
    assert_eq!(facts.probe_offset, 3);
    assert_eq!(facts.probe_end, 12);
}
