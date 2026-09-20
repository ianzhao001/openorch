//! Real parser and bounded native-query evidence, without model inference.
use orch_host::native_discovery::{
    discover_with_context, opencode_local_readiness, parse_catalog_output, parse_native_config,
    DiscoveryContext, OpenCodeReadiness,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
fn repo() -> PathBuf {
    let r = orch_host::util::test_scratch_dir("b359-native");
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    ] {
        assert!(Command::new("git")
            .arg("-c")
            .arg("core.fsmonitor=false")
            .args(args)
            .current_dir(&r)
            .status()
            .unwrap()
            .success());
    }
    r
}

#[test]
fn opencode_readiness_is_read_only_and_fail_closed_only_on_definite_facts() {
    let r = repo();
    let c = context(&r);
    let before = supertree(&c.home);
    assert!(matches!(opencode_local_readiness(&c), OpenCodeReadiness::Indeterminate { .. }));
    assert_eq!(before, supertree(&c.home));
    let data = c.home.join(".local/share/opencode");
    let state = c.home.join(".local/state/opencode");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(matches!(opencode_local_readiness(&c), OpenCodeReadiness::Unsafe { .. }));
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(&data).unwrap();
    assert_eq!(opencode_local_readiness(&c), OpenCodeReadiness::Ready);
    fs::set_permissions(&state, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(matches!(opencode_local_readiness(&c), OpenCodeReadiness::Unsafe { .. }));
}

fn supertree(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(dir) = pending.pop() {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                paths.push(entry.path());
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
    }
    paths.sort();
    paths
}
fn context(root: &Path) -> DiscoveryContext {
    let h = root.join("home");
    let bin = root.join("bin");
    fs::create_dir_all(&h).unwrap();
    fs::create_dir_all(&bin).unwrap();
    DiscoveryContext {
        project: root.to_owned(),
        home: h,
        search_path: vec![bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
        query_timeout_ms: 5000,
        allow_commands: false,
        overrides: BTreeMap::new(),
        include_platform_locations: false,
    }
}
fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn exe(ctx: &DiscoveryContext, name: &str, body: &str) {
    let p = ctx.search_path[0].join(name);
    fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}
#[test]
fn native_effort_fields_are_client_specific_and_not_guessed() {
    let c=parse_native_config("claude",r#"{"model":"m","effort":"wrong","effortLevel":"high","modelSettings":{"m":{"effortLevel":"future"}},"apiKey":"credential-canary"}"#).unwrap();
    assert_eq!(c.current.effort.as_deref(), Some("future"));
    let b = parse_native_config(
        "codebuddy",
        r#"{"model":"m","effortLevel":"wrong","effort":"wrong","reasoningEffort":"native-new"}"#,
    )
    .unwrap();
    assert_eq!(b.current.effort.as_deref(), Some("native-new"));
    let p=parse_native_config("pi",r#"{"defaultProvider":"p","defaultModel":"m","defaultThinkingLevel":"off","providers":{"p":{"apiKey":"credential-canary","models":[{"id":"m","thinkingLevelMap":{"off":{},"brand-new":{}}}]}}}"#).unwrap();
    assert_eq!(p.current.effort.as_deref(), Some("off"));
    assert!(p.models[0].efforts.contains(&"brand-new".into()));
    let d=parse_native_config("dsh","agent-default-model:\n  provider: p\n  model: m\n  reasoningEffort: native-effort\ncredentials: do-not-copy\n").unwrap();
    assert_eq!(d.current.effort.as_deref(), Some("native-effort"));
    for x in [c, b, p, d] {
        let s = serde_json::to_string(&x).unwrap();
        assert!(!s.contains("credential-canary") && !s.contains("do-not-copy"));
    }
}
#[test]
fn codex_and_json_comments_do_not_confuse_sections_or_quoted_values() {
    let c=parse_native_config("codex","model = \"native/#model\" # comment\nmodel_reasoning_effort='future'\n[unrelated]\nmodel=\"not-current\"\n").unwrap();
    assert_eq!(c.current.model.as_deref(), Some("native/#model"));
    assert_eq!(c.current.effort.as_deref(), Some("future"));
    assert!(parse_native_config("codex", "profile='old-selector'\nmodel='m'\n").is_err());
    let c=parse_native_config("opencode",r#"{/*comment*/"model":"vendor/model/*literal*/", "endpoint":"https://private", "apiKey":"secret"}// end"#).unwrap();
    assert_eq!(c.current.model.as_deref(), Some("model/*literal*/"));
    assert!(!serde_json::to_string(&c).unwrap().contains("private"));
}
#[test]
fn catalog_projection_does_not_copy_verbose_options_or_normalize_variants() {
    let raw="vendor/m\n{\"providerID\":\"vendor\",\"id\":\"m\",\"name\":\"Model\",\"api\":{\"headers\":{\"Authorization\":\"secret-canary\"}},\"variants\":{\"future-effort\":{\"token\":\"secret-canary\"}}}\n";
    for driver in ["opencode", "mimo"] {
        let m = parse_catalog_output(driver, raw).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].efforts, vec!["future-effort"]);
        assert!(!serde_json::to_string(&m).unwrap().contains("secret-canary"));
    }
    let c=parse_catalog_output("codex",r#"{"models":[{"slug":"future-model","display_name":"Model","default_reasoning_level":"native","supported_reasoning_levels":[{"effort":"native"}]}]}"#).unwrap();
    assert_eq!(c[0].id, "future-model");
    assert_eq!(
        parse_catalog_output("agy", "native-model\tNative label\n").unwrap()[0].id,
        "native-model"
    );
    assert_eq!(
        parse_catalog_output("codebuddy", "Currently supported: first, new-model)")
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn real_discovery_observes_layers_changes_and_isolates_bad_native_files() {
    let r = repo();
    let c = context(&r);
    exe(&c, "codex", "exit 99");
    exe(&c, "claude", "exit 99");
    put(&c.home.join(".codex/config.toml"), "model='global'\n");
    put(&r.join(".codex/config.toml"), "model='project'\n");
    put(&c.home.join(".claude/settings.json"), "{");
    let first = discover_with_context(&c).unwrap();
    let codex = first
        .harnesses
        .iter()
        .find(|h| h.driver == "codex")
        .unwrap();
    assert_eq!(codex.native.current.model.as_deref(), Some("project"));
    assert_eq!(
        first
            .harnesses
            .iter()
            .find(|h| h.driver == "claude")
            .unwrap()
            .native
            .status,
        "unavailable"
    );
    put(&r.join(".codex/config.toml"), "model='changed'\n");
    let next = discover_with_context(&c).unwrap();
    assert_eq!(
        next.harnesses
            .iter()
            .find(|h| h.driver == "codex")
            .unwrap()
            .native
            .current
            .model
            .as_deref(),
        Some("changed")
    );
    assert!(
        !r.join(".orch").exists(),
        "file-only scan must not create state"
    );
}
#[test]
fn actual_query_receives_only_native_catalog_arguments_and_keeps_home_settings() {
    let r = repo();
    let mut c = context(&r);
    c.allow_commands = true;
    exe(
        &c,
        "codex",
        r#"[ "$*" = "debug models --bundled" ] || exit 99
printf '%s' '{"models":[{"slug":"from-real-child","supported_reasoning_levels":[{"effort":"new"}]}]}'"#,
    );
    let path = c.home.join(".codex/config.toml");
    put(&path, "model='current'\n");
    let before = fs::read(&path).unwrap();
    let result = discover_with_context(&c).unwrap();
    let row = result
        .harnesses
        .iter()
        .find(|h| h.driver == "codex")
        .unwrap();
    assert!(!row.native.models.is_empty(), "{row:?}");
    assert_eq!(row.native.models[0].id, "from-real-child");
    assert_eq!(fs::read(path).unwrap(), before);
    assert_eq!(
        fs::read_dir(r.join(".orch/native-discovery"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn slow_query_is_bounded_and_native_symlinks_never_supply_a_current_value() {
    let r = repo();
    let mut c = context(&r);
    c.allow_commands = true;
    c.query_timeout_ms = 60;
    exe(&c, "codex", "/bin/sleep 10");
    let start = Instant::now();
    let result = discover_with_context(&c).unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(result.harnesses[0]
        .native
        .diagnostics
        .iter()
        .any(|d| d.contains("query-")));
    c.allow_commands = false;
    let secret = r.join("outside");
    put(&secret, "model='must-not-read'\n");
    let p = c.home.join(".codex/config.toml");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(secret, &p).unwrap();
    let result = discover_with_context(&c).unwrap();
    let native = &result
        .harnesses
        .iter()
        .find(|h| h.driver == "codex")
        .unwrap()
        .native;
    assert!(native.current.model.is_none());
    assert_eq!(native.status, "unavailable");
}
#[test]
fn explicit_path_override_is_read_without_changing_process_environment() {
    let r = repo();
    let mut c = context(&r);
    exe(&c, "codex", "exit 99");
    let home = r.join("different-native-home");
    put(&home.join("config.toml"), "model='override'\n");
    c.overrides
        .insert("CODEX_HOME".into(), home.display().to_string());
    let result = discover_with_context(&c).unwrap();
    assert_eq!(
        result.harnesses[0].native.current.model.as_deref(),
        Some("override")
    );
}

#[test]
fn dsh_profile_selects_its_actual_settings_file_without_reading_credentials() {
    let r = repo();
    let mut c = context(&r);
    c.allow_commands = true;
    exe(
        &c,
        "dsh",
        r#"[ "$*" = "--profile headless --dump-config" ] || exit 99
printf '%s\n' '- id: settings' '  config:' '    path: custom-settings.yaml' '- id: credentials' '  config: secret-canary'"#,
    );
    put(
        &c.home.join(".dsh/settings.yaml"),
        "agent-default-model: {provider: wrong, model: stale}\n",
    );
    put(
        &r.join("custom-settings.yaml"),
        "agent-default-model: {provider: native, model: actual, reasoningEffort: opaque}\n",
    );
    let result = discover_with_context(&c).unwrap();
    let row = result.harnesses.iter().find(|h| h.driver == "dsh").unwrap();
    assert_eq!(row.native.current.model.as_deref(), Some("actual"));
    assert_eq!(row.native.current.mode.as_deref(), Some("headless"));
    assert!(!serde_json::to_string(&result)
        .unwrap()
        .contains("secret-canary"));
    assert!(row.sources.iter().any(|s| s.kind == "native-command"));
}

#[test]
fn jsonc_trailing_commas_are_supported_without_rewriting_model_strings() {
    let value =
        parse_native_config("mimo", r#"{"model":"native/model,}", /* comment */}"#).unwrap();
    assert_eq!(value.current.model.as_deref(), Some("native/model,}"));
    assert!(parse_native_config("mimo", "{} /* unfinished").is_err());
}

#[test]
#[ignore = "explicit installed-client readback; does not invoke models"]
fn installed_native_discovery_readback() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let result = orch_host::native_discovery::discover(root).unwrap();
    let output = std::env::var("B359_NATIVE_PROOF").expect("explicit proof output path");
    fs::write(output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    for row in &result.harnesses {
        println!(
            "{} {} current={} models={} diagnostics={:?}",
            row.id,
            row.native.status,
            row.native.current.model.is_some(),
            row.native.models.len(),
            row.native.diagnostics
        );
    }
    assert!(result.harnesses.iter().any(|h| h.id == "installed:codex"));
}

#[test]
fn disabled_alias_is_not_queried_and_metadata_never_grants_consult_permission() {
    let r = repo();
    let mut c = context(&r);
    c.allow_commands = true;
    let disabled = c.search_path[0].join("private-disabled");
    fs::write(&disabled, "#!/bin/sh\ntouch disabled-was-called\n").unwrap();
    fs::set_permissions(&disabled, fs::Permissions::from_mode(0o755)).unwrap();
    put(&r.join(".gitignore"), ".orch/\n");
    put(&r.join(".orch/harnesses.yaml"),&serde_json::json!({"version":1,"harnesses":{"held":{"driver":"agy","executable":disabled,"enabled":false,"cwdPolicy":"project-root","defaults":{}}}}).to_string());
    let result = discover_with_context(&c).unwrap();
    let row = result
        .harnesses
        .iter()
        .find(|h| h.id == "configured:held")
        .unwrap();
    assert!(!row.enabled);
    assert_eq!(row.availability, "unsupported");
    assert!(!r.join("disabled-was-called").exists());
    let config = orch_host::harness_config::load_harness_config_snapshot(&r).unwrap();
    assert!(config
        .resolve("held", orch_host::harness_config::HarnessAction::Consult)
        .is_err());
}
#[test]
fn native_cache_is_catalog_data_and_later_explicit_model_definition_wins() {
    let r = repo();
    let c = context(&r);
    exe(&c, "pi", "exit 99");
    put(
        &c.home.join(".pi/agent/models-store.json"),
        r#"{"vendor":{"models":[{"id":"m","name":"cached"}]}}"#,
    );
    put(
        &c.home.join(".pi/agent/models.json"),
        r#"{"providers":{"vendor":{"models":[{"id":"m","name":"explicit","thinkingLevelMap":{"off":{}}}]}}}"#,
    );
    let result = discover_with_context(&c).unwrap();
    let row = result.harnesses.iter().find(|h| h.driver == "pi").unwrap();
    assert_eq!(row.native.models.len(), 1);
    assert_eq!(row.native.models[0].name, "explicit");
    assert!(row.sources.iter().any(|s| s.kind == "native-cache"));
}
#[test]
fn model_specific_effort_layer_and_session_override_are_distinct() {
    let r = repo();
    let mut c = context(&r);
    exe(&c, "claude", "exit 99");
    put(
        &c.home.join(".claude/settings.json"),
        r#"{"model":"m","effortLevel":"low"}"#,
    );
    put(
        &r.join(".claude/settings.local.json"),
        r#"{"modelSettings":{"m":{"effortLevel":"future"}}}"#,
    );
    let result = discover_with_context(&c).unwrap();
    assert_eq!(
        result.harnesses[0].native.current.effort.as_deref(),
        Some("future")
    );
    c.overrides
        .insert("CLAUDE_CODE_EFFORT_LEVEL".into(), "session".into());
    let result = discover_with_context(&c).unwrap();
    assert_eq!(
        result.harnesses[0].native.current.effort.as_deref(),
        Some("session")
    );
    assert!(result.harnesses[0]
        .sources
        .iter()
        .any(|s| s.kind == "environment"));
}

#[test]
fn higher_claude_layer_top_level_beats_lower_model_specific_setting() {
    let r = repo();
    let c = context(&r);
    exe(&c, "claude", "exit 99");
    put(
        &c.home.join(".claude/settings.json"),
        r#"{"model":"m","modelSettings":{"m":{"effortLevel":"high"}}}"#,
    );
    put(
        &r.join(".claude/settings.local.json"),
        r#"{"effortLevel":"low"}"#,
    );
    let result = discover_with_context(&c).unwrap();
    assert_eq!(
        result.harnesses[0].native.current.effort.as_deref(),
        Some("low")
    );
}
#[test]
fn native_alias_ambiguity_is_delegated_instead_of_guessing_a_version_table() {
    let value=parse_native_config("claude",r#"{"model":"dated-alias","modelSettings":{"dated-alias":{"effortLevel":"high"},"canonical-model":{"effortLevel":"low"}}}"#).unwrap();
    assert!(value.current.effort.is_none());
    assert!(value
        .diagnostics
        .iter()
        .any(|s| s.contains("alias_resolution")));
}

#[test]
fn oversized_native_file_and_catalog_are_rejected_without_partial_defaults() {
    let root = repo();
    let ctx = context(&root);
    exe(&ctx, "claude", "exit 0");
    let huge = " ".repeat(2 * 1024 * 1024 + 1);
    put(&ctx.home.join(".claude/settings.json"), &huge);
    assert!(parse_native_config("claude", &huge).is_err());
    assert!(parse_catalog_output("codex", &huge).is_err());
    let scan = discover_with_context(&ctx).unwrap();
    let row = scan
        .harnesses
        .iter()
        .find(|r| r.driver == "claude")
        .unwrap();
    assert_eq!(row.native.status, "unavailable");
    assert!(row.native.current.model.is_none());
}
#[test]
fn actual_query_output_limit_and_authentication_failure_are_isolated() {
    let root = repo();
    let mut ctx = context(&root);
    ctx.allow_commands = true;
    exe(&ctx, "codex", "exec /usr/bin/yes catalog-overflow");
    exe(
        &ctx,
        "cursor-agent",
        "echo 'authentication required' >&2; exit 1",
    );
    let start = Instant::now();
    let scan = discover_with_context(&ctx).unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    let codex = scan.harnesses.iter().find(|r| r.driver == "codex").unwrap();
    assert!(codex.native.models.is_empty());
    assert!(codex
        .native
        .diagnostics
        .iter()
        .any(|d| d == "query-timeout-or-output-limit"));
    let cursor = scan
        .harnesses
        .iter()
        .find(|r| r.driver == "cursor")
        .unwrap();
    assert_eq!(cursor.native.status, "auth-required");
    assert!(cursor.native.models.is_empty());
}
