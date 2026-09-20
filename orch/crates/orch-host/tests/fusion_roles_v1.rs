//! Native configuration projection and project-local role storage.
//! Mutation obligations: leak a secret; replace native opaque effort; ignore
//! fixed override; accept stale revision; follow a symlink; mutate run input.
use orch_host::fusion_roles::{parse_native_config, resolve_tuple, FusionConfig, FusionRole, FusionCombination, load_config, save_config};
use orch_host::channel::InvocationTuple;
use serde_json::json;

#[test]
fn native_catalog_keeps_ids_without_copying_credentials() {
    let native = parse_native_config("zcode", &json!({"model":{"main":"vendor/new-model"},"provider":{"vendor":{"apiKey":"b358-secret-canary","models":{"new-model":{"name":"Native model","reasoning":{"levels":["off","future-effort"],"defaultLevel":"future-effort"}}}}}}).to_string()).unwrap();
    assert_eq!(native.current.model.as_deref(),Some("new-model"));
    assert_eq!(native.current.provider.as_deref(),Some("vendor"));
    assert_eq!(native.current.effort.as_deref(),Some("future-effort"));
    assert!(native.models.iter().any(|model| model.id=="new-model" && model.efforts.contains(&"off".to_string())));
    assert!(!serde_json::to_string(&native).unwrap().contains("b358-secret-canary"));
}

#[test]
fn follow_and_fixed_fields_have_distinct_semantics() {
    let native=InvocationTuple{provider:Some("p".into()),model:Some("fresh".into()),effort:Some("off".into()),mode:None};
    let fixed=InvocationTuple{model:Some("fixed".into()),..Default::default()};
    let resolved=resolve_tuple(&native,&fixed);
    assert_eq!(resolved.model.as_deref(),Some("fixed"));
    assert_eq!(resolved.effort.as_deref(),Some("off"));
    assert_eq!(resolve_tuple(&native,&InvocationTuple::default()),native);
}

fn repository() -> std::path::PathBuf {
    let root=orch_host::util::test_scratch_dir("b358-role-store");
    std::fs::create_dir_all(&root).unwrap();
    assert!(std::process::Command::new("git").args(["init","-q"]).arg(&root).status().unwrap().success());
    std::fs::write(root.join(".gitignore"),".orch/\n").unwrap();
    for args in [vec!["add",".gitignore"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","fixture"]] {
        assert!(std::process::Command::new("git").args(args).current_dir(&root).status().unwrap().success());
    }
    root
}
fn configuration() -> FusionConfig {
    FusionConfig{revision:0,roles:vec![FusionRole{id:"analyst".into(),name:"Analyst".into(),instructions:"Inspect evidence".into(),harness:"codex".into(),fixed:InvocationTuple::default()}],combinations:vec![FusionCombination{id:"default".into(),name:"Default".into(),members:vec!["analyst".into()],disabled:vec![],synthesizer:Some("analyst".into())}]}
}
#[test]
fn project_store_is_atomic_and_rejects_stale_writers() {
    let root=repository();let config=configuration();
    let saved=save_config(&root,0,&config).unwrap();assert_eq!(saved.revision,1);
    assert_eq!(load_config(&root).unwrap(),saved);
    assert!(save_config(&root,0,&config).is_err());
    assert_eq!(load_config(&root).unwrap(),saved);
}
#[test]
fn duplicate_roles_and_dangling_members_are_rejected() {
    let root=repository();let mut config=configuration();config.roles.push(config.roles[0].clone());
    assert!(save_config(&root,0,&config).is_err());
    let mut config=configuration();config.combinations[0].members.push("absent".into());
    assert!(save_config(&root,0,&config).is_err());
}
#[test]
fn project_store_never_follows_symlinks() {
    use std::os::unix::fs::symlink;
    let root=repository();let outside=root.join("outside");std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside,root.join(".orch")).unwrap();
    assert!(save_config(&root,0,&configuration()).is_err());assert!(!outside.join("fusion.json").exists());
}
#[test]
fn current_only_configuration_does_not_invent_catalog_entries() {
    let native=parse_native_config("claude",r#"{"model":"native-model","apiKey":"hidden"}"#).unwrap();
    assert_eq!(native.current.model.as_deref(),Some("native-model"));assert!(native.models.is_empty());
    assert!(!serde_json::to_string(&native).unwrap().contains("hidden"));
    assert!(parse_native_config("claude","{").is_err());
}
