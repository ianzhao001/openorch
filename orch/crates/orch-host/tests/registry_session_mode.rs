//! B114 seed · explicit fresh/resume registry mode with durable overlay.
//!
//! 预期红：compile，缺少 SessionMode / SessionOverlay / resolve_session_mode。
//! M1：resume 故障后仍返回 Resume → fault_overlay_degrades_resume_to_fresh 红。
//! M2：把 overlay 写回配置而不是纯投影 → overlay_is_durable_but_config_immutable 红。
//! M3：未知/空 mode 默认 Resume → invalid_mode_fails_closed 红。

use orch_host::wake::{resolve_session_mode, SessionMode, SessionOverlay};

#[test]
fn fault_overlay_degrades_resume_to_fresh() {
    let overlay = SessionOverlay::degraded("thread-not-found", "fault-event-1").unwrap();
    let resolved = resolve_session_mode(SessionMode::Resume, Some(&overlay)).unwrap();
    assert_eq!(resolved.effective, SessionMode::Fresh);
    assert_eq!(resolved.configured, SessionMode::Resume);
    assert_eq!(resolved.overlay_event_id.as_deref(), Some("fault-event-1"));
}

#[test]
fn overlay_is_durable_but_config_immutable() {
    let overlay = SessionOverlay::degraded("context-exhausted", "fault-event-2").unwrap();
    let resolved = resolve_session_mode(SessionMode::Fresh, Some(&overlay)).unwrap();
    assert_eq!(resolved.configured, SessionMode::Fresh);
    assert_eq!(resolved.effective, SessionMode::Fresh);
}

#[test]
fn invalid_mode_fails_closed() {
    assert_eq!("fresh".parse::<SessionMode>().unwrap(), SessionMode::Fresh);
    assert_eq!("resume".parse::<SessionMode>().unwrap(), SessionMode::Resume);
    assert!("".parse::<SessionMode>().is_err());
    assert!("auto".parse::<SessionMode>().is_err());
}
