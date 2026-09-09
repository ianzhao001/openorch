//! B320 · every action consumes the same immutable driver/config snapshot.
//!
//! Expected red: compile (`E0432`) until `orch_host::channel` exists.
//! M1 reread config after render -> `render_spawn_and_receipt_share_one_captured_snapshot` companion red.
//! M2 resolve executable through PATH or accept identity drift -> `absolute_executable_drift_fails_before_spawn_without_path_fallback` red.
//! M3 import pins from ambient env -> `ambient_environment_cannot_change_or_supply_the_effective_tuple` red.
//! M4 fall back to tracked registry/adapter/preset bytes -> `v3_live_actions_have_zero_tracked_registry_fallbacks` red.
//! M5 let receipt facts drift from spawn -> `receipt_must_exactly_match_the_spawned_invocation_and_attachment_manifest` red.
//! M6 ignore cwdPolicy or review HEAD/worktree context -> `real_driver_observes_exact_project_root_or_target_worktree_cwd` red.
//! M7 ignore attachment bytes/order/symlink or reread -> `attachment_manifest_binds_order_bytes_and_one_snapshot` red.
//! M8 accept zero/two dispatch routes or let v3 wake resolve a tracked agent -> dispatch XOR test or
//! `dispatch_route_is_exactly_local_xor_harness` or `v3_wake_never_resolves_a_tracked_agent` red.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use orch_host::channel::{
    capture_attachment_manifest_v1, prepare_invocation, CwdSelection, DispatchRouteV3,
    InvocationAction, InvocationRequest, UNIFIED_CHANNEL_CONTRACT_V1,
};
use orch_host::harness_config::parse_harness_config_snapshot;

#[test]
fn prepared_invocation_binds_one_snapshot_and_effective_tuple() {
    assert_eq!(UNIFIED_CHANNEL_CONTRACT_V1, 1);
    let yaml = r#"
version: 1
harnesses:
  alpha:
    driver: opencode
    executable: /bin/echo
    enabled: true
    defaults: {provider: local, model: model-a, effort: high, mode: normal}
    execute: {effort: medium}
    cwdPolicy: project-root
"#;
    let snapshot = parse_harness_config_snapshot(Path::new("/repo/.orch/harnesses.yaml"), yaml)
        .expect("snapshot");
    let request = InvocationRequest {
        alias: "alpha".into(),
        action: InvocationAction::Execute,
        prompt: "implement the fixed head".into(),
        project_root: "/repo".into(),
        target_worktree: "/repo/.worktrees/B320".into(),
        target_head: "0123456789012345678901234567890123456789".into(),
        attachments: capture_attachment_manifest_v1(&[]).expect("empty manifest"),
    };
    let prepared = prepare_invocation(&snapshot, request).expect("prepared invocation");
    assert_eq!(prepared.executable(), Path::new("/bin/echo"));
    assert_eq!(prepared.cwd(), Path::new("/repo"));
    assert_eq!(prepared.cwd_selection(), CwdSelection::ProjectRoot);
    assert_eq!(prepared.config_digest(), snapshot.sha256());
    assert_eq!(prepared.effective().effort.as_deref(), Some("medium"));
    assert_eq!(prepared.requested().action, InvocationAction::Execute);
    assert_eq!(
        prepared.attachment_manifest_digest(),
        capture_attachment_manifest_v1(&[])
            .expect("empty manifest")
            .sha256()
    );
}

fn attachment_dir() -> PathBuf {
    std::env::current_dir()
        .expect("current dir")
        .join(".cowork-temp")
        .join(format!("B320-seed-{}", std::process::id()))
}

#[test]
fn attachment_manifest_binds_order_bytes_and_one_snapshot() {
    let dir = attachment_dir();
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("fixture dir");
    let a = dir.join("a.md");
    let b = dir.join("b.md");
    fs::write(&a, b"alpha").expect("write a");
    fs::write(&b, b"beta").expect("write b");

    let ab = capture_attachment_manifest_v1(&[a.as_path(), b.as_path()]).expect("ab");
    let ba = capture_attachment_manifest_v1(&[b.as_path(), a.as_path()]).expect("ba");
    assert_ne!(
        ab.sha256(),
        ba.sha256(),
        "attachment order is request truth"
    );

    let in_flight = ab.sha256().to_string();
    fs::write(&a, b"alpha-mutated").expect("mutate a");
    assert_eq!(ab.sha256(), in_flight, "captured action must not reread");
    let next = capture_attachment_manifest_v1(&[a.as_path(), b.as_path()]).expect("next");
    assert_ne!(
        next.sha256(),
        in_flight,
        "next action must observe new bytes"
    );

    #[cfg(unix)]
    {
        let link = dir.join("linked.md");
        symlink(&b, &link).expect("make symlink");
        assert!(capture_attachment_manifest_v1(&[link.as_path()]).is_err());
    }
    fs::remove_dir_all(&dir).expect("clean fixture");
}

#[test]
fn dispatch_route_is_exactly_local_xor_harness() {
    assert!(DispatchRouteV3::from_flags(false, None).is_err());
    assert!(DispatchRouteV3::from_flags(true, Some("alpha")).is_err());
    assert!(matches!(
        DispatchRouteV3::from_flags(true, None).expect("local"),
        DispatchRouteV3::Local
    ));
    assert!(matches!(
        DispatchRouteV3::from_flags(false, Some("alpha")).expect("harness"),
        DispatchRouteV3::Harness(alias) if alias == "alpha"
    ));
}
