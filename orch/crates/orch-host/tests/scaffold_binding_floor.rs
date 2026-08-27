//! B306 · generated Rust bindings must satisfy the production gate floor.
//!
//! This exercises the real handoff boundary: detect ecosystem markers with
//! `scaffold::bind`, persist the proposal unchanged, then reload it through
//! `binding::load`. Both Rust-only and mixed projects must keep an independent
//! `--all-targets` token in their Cargo check command.

use std::fs;
use std::path::{Path, PathBuf};

use orch_host::{binding, scaffold};

struct ScaffoldRoot(PathBuf);

impl ScaffoldRoot {
    fn new(tag: &str, mixed: bool) -> Self {
        let root = orch_host::util::test_scratch_dir(tag);
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        if mixed {
            fs::write(root.join("package.json"), "{}\n").unwrap();
        }
        scaffold::init(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScaffoldRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn persist_and_load(root: &Path) -> (String, binding::Binding) {
    let proposal = scaffold::bind(root).expect("ecosystem markers must produce a proposal");
    fs::write(
        root.join("coordination/PROJECT-BINDING.yaml"),
        &proposal.yaml,
    )
    .unwrap();
    let loaded =
        binding::load(root).expect("an unchanged scaffold proposal must satisfy the current floor");
    (proposal.yaml, loaded)
}

fn assert_exact_check_argv(proposal: &str, binding: &binding::Binding, command: &str) {
    let expected = format!(
        "  {command}:\n    argv: [\"cargo\", \"check\", \"--workspace\", \"--all-targets\", \"--locked\"]"
    );
    assert!(
        proposal.contains(&expected),
        "scaffold proposal must contain exact {command} argv"
    );

    let argv = &binding
        .commands
        .get(command)
        .unwrap_or_else(|| panic!("generated binding must contain {command}"))
        .argv;
    let cargo_args = argv.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
    assert_eq!(
        cargo_args,
        ["check", "--workspace", "--all-targets", "--locked"],
        "machine overlay may replace argv[0], but not {command}'s Cargo arguments"
    );
    let terminator = argv
        .iter()
        .position(|token| token == "--")
        .unwrap_or(argv.len());
    assert_eq!(
        argv[..terminator]
            .iter()
            .filter(|token| token.as_str() == "--all-targets")
            .count(),
        1,
        "{command} must preserve one independent pre-terminator --all-targets token"
    );
}

#[test]
fn rust_only_scaffold_round_trips_through_current_binding_floor() {
    let root = ScaffoldRoot::new("b306-scaffold-rust", false);
    let (proposal, loaded) = persist_and_load(root.path());

    assert_eq!(loaded.project.ecosystems, vec!["rust"]);
    assert_exact_check_argv(&proposal, &loaded, "check");
}

#[test]
fn mixed_scaffold_round_trips_through_current_binding_floor() {
    let root = ScaffoldRoot::new("b306-scaffold-mixed", true);
    let (proposal, loaded) = persist_and_load(root.path());

    assert_eq!(loaded.project.ecosystems, vec!["rust", "node"]);
    assert_exact_check_argv(&proposal, &loaded, "rustCheck");
}
