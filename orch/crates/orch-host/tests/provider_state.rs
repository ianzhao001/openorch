//! B111 seed · provider state normalization.
//!
//! 预期红：compile，缺少 ProviderKind / ProviderState / normalize_provider_state。
//! M1：Engaged 优先于 Fault → fault_always_wins 红。
//! M2：把 spawn/pid alive 当 Engaged → spawn_is_only_pending 红。
//! M3：漏掉 dclaw 或任一 provider → all_five_providers_share_one_state_machine 红。

use orch_host::chanhealth::{normalize_provider_state, ProviderKind, ProviderState};

#[test]
fn fault_always_wins() {
    let state = normalize_provider_state(
        ProviderKind::OpenCode,
        true,
        true,
        "! permission requested: external_directory; auto-rejecting",
    );
    assert!(matches!(state, ProviderState::Fault(_)));
}

#[test]
fn spawn_is_only_pending() {
    assert_eq!(
        normalize_provider_state(ProviderKind::Codex, true, false, ""),
        ProviderState::Pending
    );
    assert_eq!(
        normalize_provider_state(ProviderKind::Codex, true, true, "turn.started"),
        ProviderState::Engaged
    );
}

#[test]
fn all_five_providers_share_one_state_machine() {
    let kinds = [
        ProviderKind::Codex,
        ProviderKind::OpenCode,
        ProviderKind::SmartClaw,
        ProviderKind::Agy,
        ProviderKind::Dclaw,
    ];
    assert_eq!(kinds.len(), 5);
    for kind in kinds {
        assert_eq!(
            normalize_provider_state(kind, false, false, ""),
            ProviderState::Pending
        );
    }
}
