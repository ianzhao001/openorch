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
    let live = host
        .split_once("fn run_channel_consultation_v3")
        .expect("live consult function")
        .1
        .split_once("#[derive(Debug)]\nstruct LoadedQuestion")
        .expect("live consult end")
        .0;
    assert_eq!(
        live.matches("load_harness_config_snapshot(root)?").count(),
        1,
        "one consultation action must read one config snapshot"
    );
    assert!(live.contains("std::thread::scope"));
    assert!(!live.contains("strip_prefix(\"consult-\")"));
}
