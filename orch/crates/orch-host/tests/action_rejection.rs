//! B113 seed · durable action-scoped rejection.
//!
//! 预期红：compile，缺少 ActionRejection / rejection_event。
//! M1：exitCode=0 → rejection_is_nonzero_and_action_scoped 红。
//! M2：遗漏 actionId → rejection_is_nonzero_and_action_scoped 红。
//! M3：允许空 operation/reason → malformed_rejection_is_rejected 红。

use orch_host::failure::{rejection_event, ActionRejection};

#[test]
fn rejection_is_nonzero_and_action_scoped() {
    let rejection = ActionRejection::new("wake", "wake-123", "registry invalid", 2).unwrap();
    let event = rejection_event("r47", Some("B113"), &rejection);
    assert_eq!(event.kind, "ActionRejected");
    let payload = event.payload.unwrap();
    assert_eq!(payload["actionId"], "wake-123");
    assert_eq!(payload["operation"], "wake");
    assert_eq!(payload["exitCode"], 2);
    assert_eq!(payload["alert"], true);
}

#[test]
fn malformed_rejection_is_rejected() {
    assert!(ActionRejection::new("", "act", "reason", 2).is_err());
    assert!(ActionRejection::new("wake", "", "reason", 2).is_err());
    assert!(ActionRejection::new("wake", "act", "", 2).is_err());
    assert!(ActionRejection::new("wake", "act", "reason", 0).is_err());
}
