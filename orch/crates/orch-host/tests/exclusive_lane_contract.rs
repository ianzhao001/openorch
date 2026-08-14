use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SELF_SOURCE: &str = include_str!("exclusive_lane_contract.rs");
const BINDING_SOURCE: &str = include_str!("../../../../coordination/PROJECT-BINDING.yaml");
const RUNNER_SOURCE: &str = include_str!("../../../scripts/test-exclusive.sh");
const FOUR_SIGNAL_SOURCE: &str = include_str!("../../orch-cli/tests/wake_signal_isolation_cli.rs");

fn assert_contract_is_default_lane() {
    let ignore_prefix = ["#[", "ignore"].concat();
    assert!(
        SELF_SOURCE
            .lines()
            .all(|line| !line.trim_start().starts_with(&ignore_prefix)),
        "exclusive lane contract must remain in the default test lane"
    );
}

fn expected_cases() -> BTreeSet<String> {
    [
        "gate::tests::registry_reap_executes_on_the_normal_gate_exit_at_runtime",
        "gate::tests::registry_reap_executes_on_the_timeout_gate_exit_at_runtime",
        "four_signal_topology_cells_pass_ten_consecutive_runs",
        "successful_kill_converges_under_pressure",
        "serve::tests::production_entries_visit_all_six_unattended_stations",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn expected_runner_calls() -> BTreeSet<String> {
    [
        "run_exact orch-host --lib _ gate::tests::registry_reap_executes_on_the_normal_gate_exit_at_runtime",
        "run_exact orch-host --lib _ gate::tests::registry_reap_executes_on_the_timeout_gate_exit_at_runtime",
        "run_exact orch-cli --test wake_signal_isolation_cli four_signal_topology_cells_pass_ten_consecutive_runs",
        "run_exact orch-host --test gate_reap_convergence successful_kill_converges_under_pressure",
        "run_exact orch-host --lib _ serve::tests::production_entries_visit_all_six_unattended_stations",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn runner_calls() -> Vec<String> {
    RUNNER_SOURCE
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("run_exact "))
        .map(str::to_owned)
        .collect()
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read Rust source directory {}: {error}", dir.display()))
    {
        let path = entry.expect("read Rust source entry").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && path.file_name().and_then(|name| name.to_str()) != Some("exclusive_lane_contract.rs")
        {
            files.push(path);
        }
    }
}

fn exclusive_ignore_markers() -> Vec<String> {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("orch-host must live below crates");
    let mut files = Vec::new();
    collect_rust_files(crates_dir, &mut files);
    let prefix = ["#[ignore = \"", "testExclusive:"].concat();
    files
        .into_iter()
        .flat_map(|path| {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            source
                .lines()
                .filter_map(|line| {
                    line.trim()
                        .strip_prefix(&prefix)
                        .and_then(|name| name.strip_suffix("\"]"))
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn function_region<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let tail = source
        .split_once(start)
        .unwrap_or_else(|| panic!("missing function anchor {start}"))
        .1;
    tail.split_once(end)
        .unwrap_or_else(|| panic!("missing function end anchor {end}"))
        .0
}

#[test]
fn binding_runner_allowlist_and_ignore_markers_are_exact() {
    assert_contract_is_default_lane();

    let binding: serde_yaml::Value = serde_yaml::from_str(BINDING_SOURCE).unwrap();
    let exclusive = &binding["commands"]["testExclusive"];
    let argv = exclusive["argv"]
        .as_sequence()
        .expect("testExclusive argv must be a sequence")
        .iter()
        .map(|value| value.as_str().expect("testExclusive argv must be strings"))
        .collect::<Vec<_>>();
    assert_eq!(
        argv,
        [
            "/bin/sh",
            "orch/scripts/test-exclusive.sh",
            "/Users/admin/.cargo/bin/cargo",
        ]
    );
    assert!(Path::new(argv[2]).is_absolute());
    assert_eq!(exclusive["timeoutSeconds"].as_u64(), Some(1200));
    for gate in ["fast", "merge"] {
        let names = binding["gates"][gate]
            .as_sequence()
            .expect("gate bundle must be a sequence")
            .iter()
            .map(|value| value.as_str().expect("gate name must be a string"))
            .collect::<Vec<_>>();
        assert_eq!(names, ["testFast", "testExclusive", "check"]);
    }

    let calls = runner_calls();
    let call_set = calls.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(calls.len(), 5, "runner must contain exactly five calls");
    assert_eq!(call_set.len(), calls.len(), "runner calls must be unique");
    assert_eq!(call_set, expected_runner_calls());
    let runner_names = calls
        .iter()
        .map(|line| line.split_whitespace().last().unwrap().to_owned())
        .collect::<BTreeSet<_>>();

    let markers = exclusive_ignore_markers();
    let marker_set = markers.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        markers.len(),
        5,
        "there must be exactly five ignore markers"
    );
    assert_eq!(
        marker_set.len(),
        markers.len(),
        "ignore markers must be unique"
    );
    assert_eq!(marker_set, runner_names);
    assert_eq!(marker_set, expected_cases());
}

#[test]
fn runner_and_four_signal_budgets_fail_closed_without_shrinking() {
    assert_contract_is_default_lane();

    assert_eq!(
        RUNNER_SOURCE
            .matches("test --locked --manifest-path orch/Cargo.toml")
            .count(),
        4,
        "every list/run cargo site must be locked and use the outer-root manifest path"
    );
    assert_eq!(
        RUNNER_SOURCE.matches("-- --list --ignored --exact").count(),
        2
    );
    assert_eq!(
        RUNNER_SOURCE
            .matches("-- --ignored --exact --test-threads=1")
            .count(),
        2
    );
    assert!(RUNNER_SOURCE.contains("[ \"$match_count\" -ne 1 ]"));
    assert!(!RUNNER_SOURCE.contains("--workspace"));

    for contract in [
        "const PROXY_PHASE_BUDGET: Duration = Duration::from_secs(10);",
        "const PROXY_CELL_OVERALL_BUDGET: Duration = Duration::from_secs(30);",
        "const MANAGED_PHASE_BUDGET: Duration = Duration::from_secs(12);",
        "const MANAGED_CELL_OVERALL_BUDGET: Duration = Duration::from_secs(36);",
        "(Instant::now() + phase_budget).min(cell_overall_deadline)",
        "for iteration in 0..10",
        "for signal in [SIGINT, SIGHUP]",
    ] {
        assert!(
            FOUR_SIGNAL_SOURCE.contains(contract),
            "four-signal contract missing {contract:?}"
        );
    }
    let proxy = function_region(
        FOUR_SIGNAL_SOURCE,
        "fn run_proxy_cell",
        "fn run_managed_cell",
    );
    let managed = function_region(
        FOUR_SIGNAL_SOURCE,
        "fn run_managed_cell",
        "fn four_signal_topology_cells_pass_ten_consecutive_runs",
    );
    assert_eq!(proxy.matches("bounded_phase_deadline(").count(), 5);
    assert_eq!(managed.matches("bounded_phase_deadline(").count(), 5);
    assert_eq!(
        proxy
            .matches("Instant::now() + PROXY_CELL_OVERALL_BUDGET")
            .count(),
        1
    );
    assert_eq!(
        managed
            .matches("Instant::now() + MANAGED_CELL_OVERALL_BUDGET")
            .count(),
        1
    );
    assert_eq!(managed.matches("completion_deadline").count(), 3);
    assert_eq!(managed.matches("group_gone_deadline").count(), 3);
    let matrix = FOUR_SIGNAL_SOURCE
        .split_once("fn four_signal_topology_cells_pass_ten_consecutive_runs")
        .expect("missing four-signal matrix")
        .1;
    assert_eq!(
        matrix.matches("run_proxy_cell(iteration, signal)").count(),
        1
    );
    assert_eq!(
        matrix
            .matches("run_managed_cell(iteration, signal)")
            .count(),
        1
    );
}
