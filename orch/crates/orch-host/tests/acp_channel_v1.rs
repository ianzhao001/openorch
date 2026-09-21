//! Shared-core integration through the actual sibling orch-acp binary.
//! Build workspace binaries before running this integration target alone.
use orch_host::{harness_config::{parse_harness_config_snapshot,HarnessAction},fusion_run::{FusionEngine,ConsultRequest},fusion_roles::FusionRole,channel::InvocationTuple,native_discovery::{DiscoveryContext,parse_native_config}};
use std::{fs,path::PathBuf,process::Command,time::{Duration,Instant},os::unix::fs::PermissionsExt};
fn fixture()->(PathBuf,String) {
 let root=orch_host::util::test_scratch_dir("ACP shared core");
 for args in [vec!["init","-q"],vec!["-c","user.name=Fixture","-c","user.email=f@example.invalid","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","commit","--allow-empty","-qm","base"]] {
  assert!(Command::new("/usr/bin/git").args(args).current_dir(&root).status().unwrap().success());
 }
 fs::create_dir_all(root.join("home")).unwrap();fs::create_dir_all(root.join(".orch")).unwrap();fs::write(root.join(".gitignore"),".orch/\n").unwrap();fs::write(root.join("kind"),"good").unwrap();
 let source=include_str!("../../orch-acp/tests/acp_bridge_v1.rs");let fake=source.split("r#\"").nth(1).unwrap().split("\"#").next().unwrap();
 let fake=fake.replace("kind=(root/'kind').read_text()", "kind=(root/'kind').read_text()\nif 'debug' in sys.argv:\n print(json.dumps({'name':'plan','permission':[{'permission':'*','pattern':'*','action':'deny'}]}));sys.exit(0)");
 let executable=root.join("agent.py");fs::write(&executable,fake).unwrap();fs::set_permissions(&executable,fs::Permissions::from_mode(0o755)).unwrap();
 let config=format!("version: 1\nharnesses:\n  fixture:\n    driver: opencode\n    executable: {}\n    enabled: true\n    defaults: {{provider: deepseek, model: deepseek-flash, effort: max}}\n    consult:\n      mode: plan\n      limits: {{timeoutSeconds: 8}}\n      acp: {{agentVersion: fixture-v1}}\n    cwdPolicy: project-root\n",executable.display());
 fs::write(root.join(".orch/harnesses.yaml"),&config).unwrap();(root,config)
}
#[test]
fn acp_configuration_is_consult_only_and_strict() {
 let (root,text)=fixture();let p=root.join(".orch/harnesses.yaml");let snapshot=parse_harness_config_snapshot(&p,&text).unwrap();
 let consult=snapshot.resolve("fixture",HarnessAction::Consult).unwrap();assert_eq!(consult.acp().unwrap().agent_version,"fixture-v1");
 assert!(snapshot.resolve("fixture",HarnessAction::Execute).unwrap().acp().is_none());
 assert!(parse_harness_config_snapshot(&p,&text.replace("consult:","execute:")).is_err());
 assert!(parse_harness_config_snapshot(&p,&text.replace("agentVersion: fixture-v1","agentVersion: fixture-v1, command: evil")).is_err());
 assert!(!root.join("calls").exists());
}
#[test]
fn role_snapshot_keeps_acp_and_uses_one_verified_core_run() {
 let (root,_)=fixture();let engine=FusionEngine::with_discovery_context(DiscoveryContext{project:root.clone(),home:root.join("home"),search_path:vec!["/usr/bin".into(),"/bin".into()],query_timeout_ms:1000,allow_commands:false,overrides:Default::default(),include_platform_locations:false});
 let request=ConsultRequest{request_id:"acp-core".into(),question:"Assess supplied facts.".into(),members:vec![FusionRole{id:"alpha".into(),name:"Alpha".into(),harness:"configured:fixture".into(),instructions:"Read only.".into(),fixed:InvocationTuple::default()}],attachments:vec![]};
 engine.start_consult(&root,request.clone()).unwrap();let start=Instant::now();loop {
  let view=engine.read_status(&root,"acp-core").unwrap();
  if !matches!(view.phase.as_str(),"preparing"|"consulting") {assert_eq!(view.phase,"completed","{view:?}");break;}
  assert!(start.elapsed()<Duration::from_secs(12));std::thread::sleep(Duration::from_millis(30));
 }
 assert_eq!(engine.read_answer_page(&root,"acp-core","alpha",0,100,None).unwrap().text,"verified answer");engine.start_consult(&root,request).unwrap();
 assert_eq!(fs::read_to_string(root.join("calls")).unwrap().lines().filter(|m|*m=="session/prompt").count(),1);
 let view=engine.read(&root,"acp-core").unwrap();assert_eq!(view.members[0].channel_facts.as_ref().unwrap()["terminal"]["answerAuthority"],"bound-acp-final");
}
#[test]
fn native_provider_route_is_private_and_never_copies_credentials() {
 let native=parse_native_config("claude",r#"{"model":"opus","env":{"ANTHROPIC_BASE_URL":"https://api.deepseek.com/anthropic","ANTHROPIC_AUTH_TOKEN":"DO_NOT_COPY_CREDENTIAL"}}"#).unwrap();
 let route=native.provider_route.as_ref().unwrap();assert_eq!(route.provider,"deepseek");assert_eq!(route.url,"https://api.deepseek.com/anthropic");let public=serde_json::to_string(&native).unwrap();assert!(!public.contains("DO_NOT_COPY_CREDENTIAL"));assert!(!public.contains("provider_route"));
 let unsafe_route=parse_native_config("claude",r#"{"env":{"ANTHROPIC_BASE_URL":"https://user:secret@api.deepseek.com/anthropic"}}"#).unwrap();assert!(unsafe_route.provider_route.is_none());
 let codex=parse_native_config("codex","model_provider=\"deepseek\"\nmodel=\"deepseek-flash\"\n[model_providers.deepseek]\nbase_url=\"https://api.deepseek.com\"\nenv_key=\"DEEPSEEK_API_KEY\"\n").unwrap();assert_eq!(codex.provider_route.unwrap().credential_env.as_deref(),Some("DEEPSEEK_API_KEY"));
}
