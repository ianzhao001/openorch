//! B360 narrow role-selection and synthesis invariants. Compile-red before API.
//! Mutations: dedupe harness instead of role; admit <2; lose order/disable;
//! accept empty request; synthesize with zero valid answers; ignore HOLD.
use orch_host::fusion_roles::{FusionCombination,FusionConfig,FusionRole};
use orch_host::channel::InvocationTuple;
use orch_host::fusion_run::{select_roles, FusionRequest, synthesis_prompt};
use orch_host::consult::{MemberOutcome,MemberStatus};

fn config() -> FusionConfig {
    FusionConfig{revision:0,roles:["a","b","s"].into_iter().map(|id| FusionRole{id:id.into(),name:id.into(),instructions:format!("Perspective {id}"),harness:"installed:claude".into(),fixed:InvocationTuple::default()}).collect(),combinations:vec![FusionCombination{id:"pair".into(),name:"Pair".into(),members:vec!["b".into(),"a".into()],disabled:vec![],synthesizer:Some("s".into())}]}
}
#[test]
fn distinct_roles_on_one_harness_preserve_order() {
    let selected=select_roles(&config(),"pair").unwrap();
    assert_eq!(selected.members.iter().map(|r|r.id.as_str()).collect::<Vec<_>>(),vec!["b","a"]);
    assert_eq!(selected.members[0].harness,selected.members[1].harness);
    assert_eq!(selected.synthesizer.id,"s");
}
#[test]
fn disabled_member_and_count_are_execution_constraints() {
    let mut c=config();c.combinations[0].disabled.push("a".into());
    assert!(select_roles(&c,"pair").is_err());
    c.combinations[0].members.push("s".into());
    let selected=select_roles(&c,"pair").unwrap();
    assert_eq!(selected.members.iter().map(|r|r.id.as_str()).collect::<Vec<_>>(),vec!["b","s"]);
}
#[test]
fn missing_synthesis_and_duplicate_roles_are_rejected() {
    let mut c=config();c.combinations[0].synthesizer=None;assert!(select_roles(&c,"pair").is_err());
    let mut c=config();c.combinations[0].members.push("a".into());assert!(select_roles(&c,"pair").is_err());
    assert!(select_roles(&config(),"absent").is_err());
}
#[test]
fn request_is_bounded_and_uses_safe_idempotency_identity() {
    let mut r=FusionRequest{request_id:"r-1".into(),combination_id:"pair".into(),question:"Assess these facts".into()};
    assert!(r.validate().is_ok());
    r.request_id="../escape".into();assert!(r.validate().is_err());r.request_id="r-1".into();
    r.question=" ".into();assert!(r.validate().is_err());
    r.question="q".repeat(128*1024+1);assert!(r.validate().is_err());
}
#[test]
fn zero_trustworthy_answers_never_produce_a_synthesis_prompt() {
    let mut m=MemberOutcome::new(0,"a",MemberStatus::Failed);m.answer=Some("untrusted failed output".into());
    assert!(synthesis_prompt("question",&[m]).is_err());
    assert!(synthesis_prompt("question",&[]).is_err());
}
#[test]
fn unclosed_member_blocks_synthesis_even_with_a_valid_peer() {
    use sha2::{Digest,Sha256};
    let mut good=MemberOutcome::new(0,"a",MemberStatus::Ok);
    good.answer=Some("verified answer".into());
    good.channel_facts=Some(serde_json::json!({"stage":"completed","terminal":{"status":"answered","turnEnded":true,"managedScopeTerminated":true,"toolOnly":false,"mechanicalTerminalAbsent":false,"finalTextSha256":hex::encode(Sha256::digest(b"verified answer"))}}));
    assert!(synthesis_prompt("question",&[good.clone()]).unwrap().contains("verified answer"));
    let mut held=MemberOutcome::new(1,"b",MemberStatus::Failed);
    held.channel_facts=Some(serde_json::json!({"stage":"unclosed","terminal":{"status":"unknown","managedScopeTerminated":false}}));
    assert!(synthesis_prompt("question",&[good,held]).is_err());
}
