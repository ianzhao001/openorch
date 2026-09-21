//! B322 source-level guard against reintroducing preset or judge grouping.

use std::fs;
use std::path::Path;

#[test]
fn consult_live_surface_contains_no_preset_or_judge_grouping() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace");
    let cli = fs::read_to_string(workspace.join("orch-cli/src/main.rs")).unwrap();
    let host = fs::read_to_string(workspace.join("orch-host/src/consult.rs")).unwrap();

    for retired in [
        "load_channel_consult_preset_v3",
        "parse_consult_presets",
        "JudgeMode",
        "run_judge(",
    ] {
        assert!(
            !host.contains(retired),
            "retired host surface leaked: {retired}"
        );
    }
    for retired in ["--preset", "--judge", "--no-judge"] {
        assert!(
            !cli.contains(retired),
            "retired CLI surface leaked: {retired}"
        );
    }
    assert!(cli.contains("long = \"harness\""));
    assert!(host.contains("ExplicitConsultMembers::new(args.harnesses.clone())"));
    let shared = fs::read_to_string(workspace.join("orch-host/src/fusion_run.rs")).unwrap();
    let public = host.split_once("pub fn run_consultation(").expect("public consultation entry").1
        .split_once("\n}").expect("public consultation body").0;
    assert!(public.contains("FusionEngine::new().run_cli(root, args)"), "public CLI must use the shared lifecycle");
    let admission = shared.split_once("pub fn run_cli(").expect("shared CLI admission").1
        .split_once("fn start_inner(").expect("shared reservation").0;
    assert_eq!(admission.matches("load_harness_config_snapshot(&root)?").count(), 1,
        "one consultation action must capture one config snapshot");
    assert!(admission.contains("discover_with_config(&context,snapshot.clone())"), "discovery must reuse the admitted configuration");
    let live = host.split_once("fn run_channel_consultation_inner(").expect("prepared executor").1
        .split_once("struct LoadedQuestion").expect("executor boundary").0;
    assert!(!live.contains("load_harness_config_snapshot"), "lower executor must not reload configuration");
    assert!(!live.contains("capture_attachment_manifest_v1"), "lower executor must not reread source inputs");
    assert!(live.contains("std::thread::scope"));
    assert!(!live.contains("strip_prefix(\"consult-\")"));
}
