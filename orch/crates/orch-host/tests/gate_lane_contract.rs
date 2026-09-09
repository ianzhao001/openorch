//! 红种子契约 · B306 · candidate/merge lane 与 source-reader closure。
//!
//! 首红：compile `E0432`，V1 lane 合同尚不存在。GateExecuted 的精确十键不在本卡扩展。

use orch_host::binding::{
    resolve_gate_lane_v1, GateLaneDecisionV1, GateLaneInputsV1, GateLaneV1,
};
use orch_host::gate::GATE_LANE_CONTRACT_V1;

const BINDING: &str = include_str!("../src/binding.rs");
const GATE: &str = include_str!("../src/gate.rs");
const COLLECT: &str = include_str!("../src/collect.rs");

fn inputs(lane: GateLaneV1) -> GateLaneInputsV1 {
    GateLaneInputsV1 {
        lane,
        candidate: vec!["seedTargets".into(), "sourceReaderClosure".into(), "check".into()],
        merge: vec!["testFast".into(), "testExclusive".into(), "check".into()],
        fast: vec!["testFast".into(), "testExclusive".into(), "check".into()],
        card_fast: vec!["testFast".into(), "testExclusive".into(), "check".into()],
        known_commands: vec![
            "seedTargets".into(),
            "sourceReaderClosure".into(),
            "testFast".into(),
            "testExclusive".into(),
            "check".into(),
        ],
        seed_targets_closed: true,
        source_readers_closed: true,
    }
}

#[test]
fn the_public_contract_anchor_is_version_one() {
    assert_eq!(GATE_LANE_CONTRACT_V1, 1);
}

#[test]
fn candidate_lane_uses_the_cards_own_closed_targets() {
    assert_eq!(
        resolve_gate_lane_v1(&inputs(GateLaneV1::Candidate)).unwrap(),
        GateLaneDecisionV1::Commands(vec![
            "seedTargets".into(),
            "sourceReaderClosure".into(),
            "check".into(),
        ])
    );
}

#[test]
fn an_unknown_reader_or_command_upgrades_to_fast() {
    let mut unknown_reader = inputs(GateLaneV1::Candidate);
    unknown_reader.source_readers_closed = false;
    assert!(matches!(
        resolve_gate_lane_v1(&unknown_reader).unwrap(),
        GateLaneDecisionV1::UpgradeToFast { .. }
    ));

    let mut unknown_command = inputs(GateLaneV1::Candidate);
    unknown_command.known_commands.retain(|name| name != "sourceReaderClosure");
    assert!(matches!(
        resolve_gate_lane_v1(&unknown_command).unwrap(),
        GateLaneDecisionV1::UpgradeToFast { .. }
    ));
}

#[test]
fn merge_lane_cannot_weaken_the_signed_card_fast_set() {
    let mut weakened = inputs(GateLaneV1::Merge);
    weakened.merge.retain(|name| name != "testExclusive");
    assert!(resolve_gate_lane_v1(&weakened).is_err());
}

#[test]
fn production_consumers_and_floors_are_not_dead_helpers() {
    assert!(BINDING.contains("sourceReaderClosure"));
    assert!(COLLECT.contains("GateLaneV1::Candidate") || COLLECT.contains("GateLane::Candidate"));
    assert!(GATE.contains("WorkspaceFullPermit"));
    assert!(GATE.contains("SIG_DFL") || GATE.contains("signal"));
}

