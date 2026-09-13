//! B344: actual maintenance of equivalent retired implementation generations.
//! Mutations: M1 restore blanket overlap refusal; M2 ignore an active old owner;
//! M3 ignore foreign-round/raw claims; M4 omit dirty/fixed-head/report checks;
//! M5 ignore corrupt/WAL evidence; M6 admit complete-journal ABA;
//! M7 execute/count aliases; M8 bypass target no-symlink identity;
//! M9 omit apply-only eligibility summary; M10 remove substantive public documentation.
use orch_core::EventRecord;
use orch_host::{
    ledger,
    reclaim::{maintain_storage, maintain_storage_for_round, maintenance_summary},
    sites::{Site, SiteRole},
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
const ROUND: &str = "rGroup";
fn git(root: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn ev(task: Option<&str>, kind: &str, p: serde_json::Value) -> EventRecord {
    ledger::event(kind, "runtime:orch", task, Some(ROUND), p)
}
fn apparent(path: &Path) -> u64 {
    let m = fs::symlink_metadata(path).unwrap();
    if m.is_dir() {
        fs::read_dir(path)
            .unwrap()
            .map(|e| apparent(&e.unwrap().path()))
            .sum()
    } else {
        m.len()
    }
}
struct Fixture {
    root: PathBuf,
    events: Vec<EventRecord>,
    sites: Vec<Site>,
}
impl Fixture {
    fn new(generations: u32) -> Self {
        let root = orch_host::util::test_scratch_dir("b344-group");
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(
            root.join(".gitignore"),
            ".worktrees/\norch/target/\ncoordination/\n",
        )
        .unwrap();
        fs::write(root.join("source.txt"), "source\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "base"]);
        let mut f = Self {
            root,
            events: vec![],
            sites: vec![],
        };
        f.add_group("G1", generations);
        f
    }
    fn add_group(&mut self, task: &str, generations: u32) {
        let report = format!("coordination/rounds/{ROUND}/reports/{task}-REPORT.md");
        let path = self.root.join(&report);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("preserved report {task}\n")).unwrap();
        git(&self.root, &["add", "-f", &report]);
        git(&self.root, &["commit", "-qm", "candidate report"]);
        let head = git(&self.root, &["rev-parse", "HEAD"]);
        let wt = format!(".worktrees/{task}");
        git(
            &self.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("task/{task}"),
                &wt,
                &head,
            ],
        );
        let target = format!("{wt}/orch/target");
        fs::create_dir_all(self.root.join(&target)).unwrap();
        fs::write(self.root.join(&target).join("cache"), vec![17; 8192]).unwrap();
        let mut new = Vec::new();
        for generation in 1..=generations {
            let s = Site {
                site_id: format!("{task}-implement-local-g{generation:02}"),
                generation,
                task_id: task.into(),
                attempt_id: format!("{task}-A{generation:04}"),
                role: SiteRole::Implement,
                agent: "local".into(),
                reviewed_head: head.clone(),
                worktree: wt.clone(),
                target: target.clone(),
                wake_id: None,
            };
            self.events.push(ev(Some(task),"WorkspaceLeased",json!({"siteId":s.site_id,"generation":generation,"attemptId":s.attempt_id,"role":"implement","agent":"local","reviewedHead":head,"paths":{"worktree":wt,"target":target}})));
            new.push(s);
        }
        self.events
            .push(ev(Some(task), "MergeStarted", json!({"headSha":head})));
        let recorded = ev(
            Some(task),
            "TaskRecorded",
            json!({"postMergeGates":"all-green"}),
        );
        let anchor = recorded.event_id.clone();
        self.events.push(recorded);
        for s in &new {
            self.events.push(ev(Some(task),"SiteRetired",json!({"siteId":s.site_id,"generation":s.generation,"taskId":task,"attemptId":s.attempt_id,"role":"implement","agent":"local","wakeId":null,"trigger":"task-recorded","retireEventId":anchor})));
        }
        self.sites.extend(new);
        self.write();
    }
    fn write(&self) {
        let mut es = self.events.clone();
        es.push(ev(None, "RoundClosed", json!({"forced":false})));
        let bytes = es
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect::<String>();
        for rel in [
            format!("coordination/rounds/{ROUND}/events.jsonl"),
            format!("coordination/runtime/ledger-wal/{ROUND}.jsonl"),
        ] {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, &bytes).unwrap();
        }
    }
    fn ledger(&self) -> Vec<u8> {
        fs::read(
            self.root
                .join(format!("coordination/rounds/{ROUND}/events.jsonl")),
        )
        .unwrap()
    }
    fn wt(&self) -> PathBuf {
        self.root.join(".worktrees/G1")
    }
    fn keep(&self) {
        let before = self.ledger();
        let r = maintain_storage(&self.root, false).unwrap();
        assert!(self.wt().exists(), "{:#?}", r.items);
        assert_eq!(r.removed_logical_bytes, 0);
        assert_eq!(self.ledger(), before);
    }
}
#[test]
fn retired_two_and_three_generations_reclaim_once_and_keep_alias_journals_absent() {
    for n in [2, 3] {
        let f = Fixture::new(n);
        let history = f.ledger();
        let expected = apparent(&f.wt());
        let report = f
            .root
            .join(format!("coordination/rounds/{ROUND}/reports/G1-REPORT.md"));
        let rb = fs::read(&report).unwrap();
        let preview = maintain_storage(&f.root, true).unwrap();
        assert!(f.wt().exists());
        assert_eq!(preview.removed_logical_bytes, 0);
        assert_eq!(
            preview
                .items
                .iter()
                .filter(|i| i.kind == "site" && i.eligible)
                .count(),
            1,
            "{:#?}",
            preview.items
        );
        let r = maintain_storage(&f.root, false).unwrap();
        assert!(!f.wt().exists(), "{:#?}", r.items);
        assert_eq!(r.removed_logical_bytes, expected);
        let representative = format!("{ROUND}/G1-implement-local-g{n:02}");
        for i in r.items.iter().filter(|i| i.kind == "site") {
            if i.id == representative {
                assert_eq!(i.disposition, "removed");
                assert_eq!(i.removed_logical_bytes, expected);
            } else {
                assert_eq!(i.disposition, "held");
                assert!(!i.eligible);
                assert_eq!(i.removed_logical_bytes, 0);
                assert!(i.reason.contains("alias"));
            }
        }
        for g in 1..n {
            assert!(!f
                .root
                .join(format!(
                    "coordination/runtime/site-cleanup/{ROUND}/G1-implement-local-g{g:02}.json"
                ))
                .exists());
        }
        assert_eq!(f.ledger(), history);
        assert_eq!(fs::read(report).unwrap(), rb);
        assert_eq!(
            maintain_storage(&f.root, false)
                .unwrap()
                .removed_logical_bytes,
            0
        );
    }
}
#[test]
fn unreleased_old_generation_holds_union_but_not_independent_sibling() {
    let mut f = Fixture::new(2);
    f.add_group("S1", 1);
    f.events.retain(|e| {
        !(e.kind == "SiteRetired"
            && e.payload
                .as_ref()
                .and_then(|p| p.get("siteId"))
                .and_then(|v| v.as_str())
                == Some("G1-implement-local-g01"))
    });
    f.write();
    let before = f.ledger();
    let r = maintain_storage(&f.root, false).unwrap();
    assert!(f.wt().exists());
    assert!(!f.root.join(".worktrees/S1").exists(), "{:#?}", r.items);
    assert_eq!(f.ledger(), before);
}
#[test]
fn cross_round_claim_survives_round_filter() {
    let f = Fixture::new(2);
    let mut other = f.events.clone();
    for e in &mut other {
        e.round = Some("rForeign".into());
    }
    other.push(ledger::event(
        "RoundClosed",
        "runtime:orch",
        None,
        Some("rForeign"),
        json!({"forced":false}),
    ));
    let p = f.root.join("coordination/rounds/rForeign/events.jsonl");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(
        p,
        other
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let r = maintain_storage_for_round(&f.root, Some(ROUND), false).unwrap();
    assert!(f.wt().exists());
    assert_eq!(r.removed_logical_bytes, 0);
}
#[test]
fn dirty_head_or_report_drift_never_becomes_group_authority() {
    for mode in ["dirty", "head", "report"] {
        let f = Fixture::new(2);
        match mode {
            "dirty" => fs::write(f.wt().join("source.txt"), "dirty").unwrap(),
            "head" => {
                fs::write(f.wt().join("source.txt"), "unmerged").unwrap();
                git(&f.wt(), &["add", "source.txt"]);
                git(&f.wt(), &["commit", "-qm", "unmerged"]);
            }
            _ => fs::write(
                f.root
                    .join(format!("coordination/rounds/{ROUND}/reports/G1-REPORT.md")),
                "changed evidence",
            )
            .unwrap(),
        };
        f.keep();
    }
}
#[test]
fn malformed_ownership_and_wal_divergence_remain_holds() {
    for wal in [false, true] {
        let f = Fixture::new(2);
        let p = f.root.join(if wal {
            format!("coordination/runtime/ledger-wal/{ROUND}.jsonl")
        } else {
            format!("coordination/rounds/{ROUND}/events.jsonl")
        });
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(p)
            .unwrap()
            .write_all(b"{unknown ownership\n")
            .unwrap();
        f.keep();
    }
}
#[test]
fn mismatched_nested_claim_or_target_symlink_is_not_equivalent() {
    for mode in ["claim", "symlink"] {
        let mut f = Fixture::new(2);
        if mode == "claim" {
            let mut e = f
                .events
                .iter()
                .find(|e| e.kind == "WorkspaceLeased")
                .unwrap()
                .clone();
            e.event_id = "foreign-claim".into();
            e.payload.as_mut().unwrap()["siteId"] = json!("foreign-unknown");
            e.payload.as_mut().unwrap()["paths"]["target"] =
                json!(".worktrees/G1/orch/target/nested");
            f.events.push(e);
            f.write();
        } else {
            let target = f.wt().join("orch/target");
            fs::rename(&target, f.root.join("preserved-target")).unwrap();
            std::os::unix::fs::symlink(f.root.join("preserved-target"), target).unwrap();
        }
        f.keep();
    }
}
#[test]
fn complete_journal_with_reappeared_group_is_preserved() {
    let f = Fixture::new(2);
    let site = f.sites.last().unwrap();
    let p = f.root.join(format!(
        "coordination/runtime/site-cleanup/{ROUND}/{}.json",
        site.site_id
    ));
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let bytes=serde_json::to_vec(&json!({"version":1,"round":ROUND,"site":site,"phase":"complete","quarantine":null,"note":null})).unwrap();
    fs::write(&p, &bytes).unwrap();
    f.keep();
    assert_eq!(fs::read(p).unwrap(), bytes);
}
#[test]
fn eligibility_summary_distinguishes_preview_and_apply_without_counting_aliases() {
    let f = Fixture::new(2);
    let mut r = maintain_storage(&f.root, true).unwrap();
    let s = maintenance_summary(&r).join("\n");
    assert!(s.contains("dryRun=true"), "{s}");
    assert!(s.contains("eligibleNotReclaimed=0"), "{s}");
    for i in &mut r.items {
        i.eligible = false;
    }
    let i = r.items.iter_mut().find(|i| i.kind == "site").unwrap();
    i.eligible = true;
    i.disposition = "failed".into();
    r.dry_run = false;
    let s = maintenance_summary(&r).join("\n");
    assert!(s.contains("eligibleNotReclaimed=1"), "{s}");
    assert!(s.contains("eligible=true"), "{s}");
}
#[test]
fn blocked_task_group_remains_held() {
    let mut f = Fixture::new(2);
    f.events.push(ev(
        Some("G1"),
        "AttemptBlocked",
        json!({"attemptId":"G1-A0002","reason":"preserve failed fixture"}),
    ));
    f.write();
    f.keep();
}
#[test]
fn public_maintenance_docs_explain_all_owner_authority() {
    let source = include_str!("../src/reclaim.rs");
    let prefix = source.split("pub fn maintain_storage(").next().unwrap();
    let docs = prefix.rsplit("\n\n").next().unwrap();
    assert!(
        docs.contains("equivalent") && docs.contains("every") && docs.contains("owner"),
        "{docs}"
    );
}
