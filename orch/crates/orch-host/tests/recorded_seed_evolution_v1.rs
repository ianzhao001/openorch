//! B328: real v3 plan and candidate checks against a truly Recorded legacy target.
//! Owned clones preserve the historical objects; only their candidate branches are mutated.
use orch_host::{card, gitx, mech, plan};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const OLD_TARGET: &str = "orch/crates/orch-host/tests/frozen_contract_supersession.rs";

fn source_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .to_owned()
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn commit(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=orch-test",
            "-c",
            "user.email=orch@test.invalid",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}
struct Fixture {
    root: PathBuf,
    base: String,
    card: card::Card,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
impl Fixture {
    fn new(tag: &str) -> Self {
        let source = source_root();
        let events =
            orch_core::read_ledger(&source.join("coordination/rounds/r84/events.jsonl")).unwrap();
        assert!(events.bad_lines.is_empty());
        let base = events
            .events
            .iter()
            .find_map(|event| {
                (event.kind == "DispatchIssued" && event.task_id.as_deref() == Some("B328")).then(
                    || {
                        event.payload.as_ref().unwrap()["baseSha"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                    },
                )
            })
            .expect("B328 dispatch fixes an immutable pre-implementation base");
        let root = orch_host::util::test_scratch_dir(&format!("b328-evolution-{tag}"));
        // The scratch allocator creates an empty directory; clone accepts that exact owned path.
        git(
            &source,
            &[
                "clone",
                "--quiet",
                "--shared",
                "--branch",
                "main",
                "--single-branch",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                source.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        git(&root, &["switch", "--detach", &base]);
        git(&root, &["branch", "-f", "main", &base]);
        git(&root, &["switch", "main"]);
        // The source really was a seed of a task with a durable Recorded fact; this is not a
        // made-up "retired=true" fixture. Locate and authenticate its immutable source bytes.
        let mut recorded_owner = None;
        for round in fs::read_dir(root.join("coordination/rounds")).unwrap() {
            let round = round.unwrap().path();
            let path = round.join("events.jsonl");
            if !path.is_file() {
                continue;
            }
            let history = orch_core::read_ledger(&path).unwrap();
            assert!(history.bad_lines.is_empty());
            for event in &history.events {
                if event.kind == "SeedRelocated"
                    && event
                        .payload
                        .as_ref()
                        .is_some_and(|p| p["target"] == OLD_TARGET)
                {
                    let owner = event.task_id.as_ref().unwrap();
                    if history
                        .events
                        .iter()
                        .any(|e| e.kind == "TaskRecorded" && e.task_id.as_ref() == Some(owner))
                    {
                        recorded_owner = Some((
                            round.file_name().unwrap().to_string_lossy().to_string(),
                            owner.clone(),
                        ));
                    }
                }
            }
        }
        assert!(
            recorded_owner.is_some(),
            "fixture target must have a real Recorded seed owner"
        );
        fs::create_dir_all(root.join("coordination/runtime")).unwrap();
        fs::write(root.join("coordination/runtime/CURRENT-ROUND"), "r84\n").unwrap();
        let rel = "coordination/rounds/r84/tasks/B328.md";
        let card_bytes = fs::read(source.join(rel)).unwrap();
        fs::write(root.join(rel), &card_bytes).unwrap();
        commit(&root, "bind the current card in the owned historical fixture");
        plan::run_plan(&root).expect(
            "the real v3 planner accepts authorized Recorded targets without permanent admission",
        );
        let base = commit(&root, "bind the current plan before candidate seed relocation");
        assert_eq!(fs::read(root.join(rel)).unwrap(), card_bytes);
        let card = card::parse(rel, "B328", &fs::read_to_string(root.join(rel)).unwrap()).unwrap();
        assert_eq!(card.meta.schema_version, Some(3));
        assert!(card::path_matches(&card.meta.write_set, OLD_TARGET));
        git(&root, &["switch", "-c", "task/B328", &base]);
        for seed in &card.meta.seeds {
            let target = root.join(&seed.target);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, fs::read(root.join(&seed.src)).unwrap()).unwrap();
        }
        commit(&root, "seed(B328): relocate immutable current contract");
        Self { root, base, card }
    }
    fn check(&self) -> anyhow::Result<()> {
        mech::check(
            &self.root,
            &self.card,
            "task/B328",
            "coordination/rounds/r84/reports/B328-REPORT.md",
            Some(&self.base),
        )
        .map(|_| ())
    }
}

#[test]
fn v3_candidate_can_change_and_delete_a_recorded_target_but_not_its_own_seed() {
    let fx = Fixture::new("change-delete");
    let old = fx.root.join(OLD_TARGET);
    let mut bytes = fs::read(&old).unwrap();
    bytes.extend_from_slice(b"\n// successor evolution\n");
    fs::write(&old, bytes).unwrap();
    commit(&fx.root, "evolve Recorded target");
    fx.check()
        .expect("authorized successor edit must pass real candidate checks");
    fs::remove_file(&old).unwrap();
    commit(&fx.root, "delete Recorded target");
    fx.check()
        .expect("authorized successor deletion must pass real candidate checks");
    let seed = &fx.card.meta.seeds[0];
    let mut bytes = fs::read(fx.root.join(&seed.target)).unwrap();
    bytes.extend_from_slice(b"\n// forbidden current seed edit\n");
    fs::write(fx.root.join(&seed.target), bytes).unwrap();
    commit(&fx.root, "tamper current seed");
    let error = fx.check().unwrap_err();
    assert!(error.to_string().contains("种子字节不一致"), "{error:#}");
}

#[test]
fn v3_retirement_preserves_the_write_domain_and_rejects_new_supersession_declarations() {
    let fx = Fixture::new("domain");
    fs::write(fx.root.join("outside-write-set.txt"), "unauthorized").unwrap();
    commit(&fx.root, "outside scope");
    let error = fx.check().unwrap_err();
    assert!(error.to_string().contains("文件域越界"), "{error:#}");
    let rel = "coordination/rounds/r84/tasks/B328.md";
    let source = fs::read_to_string(fx.root.join(rel)).unwrap();
    let forged = source.replacen(
        "schemaVersion: 3",
        "schemaVersion: 3\nfrozenContractSupersessions: []",
        1,
    );
    assert!(card::parse(rel, "B328", &forged).is_err());
    assert_eq!(gitx::rev_parse(&fx.root, "main").unwrap(), fx.base);
}
