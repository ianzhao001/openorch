//! B345 real-native safe monitor retirement, including long IPC paths.
#![cfg(target_os = "macos")]
use orch_core::EventRecord;
use orch_host::{
    ledger,
    reclaim::maintain_storage,
    sites::{Site, SiteRole},
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
const ROUND: &str = "rMonitor";
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
#[allow(dead_code)]
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
        let root = orch_host::util::test_scratch_dir("b345-monitor");
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

struct Monitor { wt: PathBuf, ipc: PathBuf }
impl Monitor {
    fn command(wt: &Path, verb: &str) -> std::process::Output {
        Command::new("git").args(["--no-optional-locks", "-c", "core.fsmonitor=true", "fsmonitor--daemon", verb]).current_dir(wt).env("HOME", wt).output().unwrap()
    }
    fn start(wt: PathBuf) -> Self {
        let ipc = PathBuf::from(git(&wt, &["rev-parse", "--git-path", "fsmonitor--daemon.ipc"]));
        let ipc = if ipc.is_absolute() { ipc } else { wt.join(ipc) };
        let m = Self { wt, ipc };
        let o = Self::command(&m.wt, "start");
        assert!(o.status.success(), "real native monitor must start: {}", String::from_utf8_lossy(&o.stderr));
        assert!(m.watching()); m
    }
    fn watching(&self) -> bool {
        if !self.wt.exists() { return false; }
        let o = Self::command(&self.wt, "status");
        o.status.success() && String::from_utf8_lossy(&o.stdout).contains("is watching '")
    }
    fn stop(&self) {
        let o = Self::command(&self.wt, "stop"); assert!(o.status.success());
        let o = Self::command(&self.wt, "status"); assert_eq!(o.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&o.stdout).contains("is not watching '"));
        assert!(!self.ipc.exists());
    }
}
impl Drop for Monitor {
    fn drop(&mut self) { if self.wt.exists() { let _ = Self::command(&self.wt, "stop"); } }
}
fn pid_set(path: &Path) -> Vec<String> {
    let o = Command::new("/usr/sbin/lsof").args(["-nP", "-t", "--"]).arg(path).output().unwrap();
    String::from_utf8(o.stdout).unwrap().lines().map(str::to_owned).collect()
}
struct Opener(std::process::Child);
impl Opener {
    fn new(wt: &Path) -> Self {
        use std::io::BufRead;
        let mut c = Command::new("/bin/sh").args(["-c", "printf 'ready\\n'; read x"]).current_dir(wt).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut line = String::new(); std::io::BufReader::new(c.stdout.take().unwrap()).read_line(&mut line).unwrap(); assert_eq!(line, "ready\n"); Self(c)
    }
}
impl Drop for Opener { fn drop(&mut self) { drop(self.0.stdin.take()); let _ = self.0.wait(); } }
#[test]
fn real_deep_native_preview_is_pending_without_effect() {
    let f = Fixture::new(1); let m = Monitor::start(f.wt()); let history=f.ledger();
    let pids=pid_set(&f.wt()); assert_eq!(pids.len(),1);
    let r=maintain_storage(&f.root,true).unwrap();
    assert!(m.watching()); assert_eq!(pid_set(&f.wt()),pids); assert_eq!(history,f.ledger()); assert_eq!(r.removed_logical_bytes,0);
    let i=r.items.iter().find(|i| i.id==format!("{ROUND}/G1-implement-local-g01")).unwrap();
    assert!(i.eligible,"{i:#?}");
    assert!(i.criteria.iter().any(|c| c.name=="last-user-absent" && c.passed.is_none()),"{i:#?}");
    assert!(!f.root.join(format!("coordination/runtime/site-cleanup/{ROUND}/G1-implement-local-g01.json")).exists());
}
#[test]
fn real_native_apply_stops_exact_monitor_reclaims_once_and_never_restarts() {
    let f=Fixture::new(1); let m=Monitor::start(f.wt()); let history=f.ledger(); let bytes=apparent(&f.wt());
    let r=maintain_storage(&f.root,false).unwrap();
    assert!(!f.wt().exists(),"{:#?}",r.items); assert!(!m.ipc.exists()); assert_eq!(r.removed_logical_bytes,bytes); assert_eq!(f.ledger(),history);
    let i=r.items.iter().find(|i| i.removed_logical_bytes>0).unwrap();
    assert!(i.criteria.iter().any(|c| c.name=="native-fsmonitor-release" && c.passed==Some(true)),"{i:#?}");
    assert_eq!(maintain_storage(&f.root,false).unwrap().removed_logical_bytes,0);
}
#[test]
fn native_group_stops_once_only_after_every_owner_passes() {
    let f=Fixture::new(3); let m=Monitor::start(f.wt());
    let r=maintain_storage(&f.root,false).unwrap(); assert!(!f.wt().exists(),"{:#?}",r.items); assert!(!m.ipc.exists());
    assert_eq!(r.items.iter().filter(|i|i.removed_logical_bytes>0).count(),1);
    for g in [1,2] { assert!(!f.root.join(format!("coordination/runtime/site-cleanup/{ROUND}/G1-implement-local-g{g:02}.json")).exists()); }
}
#[test]
fn dirty_or_unreleased_old_owner_keeps_native_monitor_untouched() {
    for dirty in [false,true] {
        let mut f=Fixture::new(2);
        if dirty { fs::write(f.wt().join("source.txt"),"dirty").unwrap(); }
        else { f.events.retain(|e| !(e.kind=="SiteRetired" && e.payload.as_ref().unwrap()["generation"]==1)); f.write(); }
        let m=Monitor::start(f.wt()); let pids=pid_set(&f.wt()); f.keep(); assert!(m.watching()); assert_eq!(pid_set(&f.wt()),pids);
    }
}
#[test]
fn mixed_opener_holds_both_processes_without_partial_stop() {
    let f=Fixture::new(1); let m=Monitor::start(f.wt()); let mut extra=Opener::new(&f.wt());
    f.keep(); assert!(m.watching()); assert!(extra.0.try_wait().unwrap().is_none());
}
#[test]
fn shared_main_and_unrelated_monitor_survive_site_cleanup() {
    let mut f=Fixture::new(1); f.add_group("G2",1);
    let shared=Monitor::start(f.root.clone()); let sibling=Monitor::start(f.root.join(".worktrees/G2"));
    // An unreleased sibling remains outside the stop domain.
    f.events.retain(|e| !(e.kind=="SiteRetired" && e.task_id.as_deref()==Some("G2"))); f.write();
    let m=Monitor::start(f.wt()); let r=maintain_storage(&f.root,false).unwrap();
    assert!(!f.wt().exists(),"{:#?}",r.items); assert!(!m.ipc.exists()); assert!(shared.watching()); assert!(sibling.watching());
}
#[test]
fn natural_exit_after_preview_uses_ordinary_quiet_path() {
    let f=Fixture::new(1); let m=Monitor::start(f.wt()); let _=maintain_storage(&f.root,true).unwrap(); m.stop();
    let r=maintain_storage(&f.root,false).unwrap(); assert!(!f.wt().exists(),"{:#?}",r.items);
    assert!(!r.items.iter().any(|i|i.criteria.iter().any(|c|c.name=="native-fsmonitor-release" && c.passed==Some(true))));
}
#[test]
fn corrupt_ownership_keeps_native_monitor_untouched() {
    let f=Fixture::new(1); let m=Monitor::start(f.wt());
    let p=f.root.join(format!("coordination/runtime/ledger-wal/{ROUND}.jsonl")); fs::write(p,"corrupt\n").unwrap();
    let r=maintain_storage(&f.root,false).unwrap(); assert!(f.wt().exists()); assert!(m.watching()); assert_eq!(r.removed_logical_bytes,0);
}
#[test]
fn public_docs_describe_native_stop_and_pending_preview() {
    let source=include_str!("../src/reclaim.rs"); let at=source.find("pub fn maintain_storage(").unwrap();
    let docs=&source[source[..at].rfind("///").unwrap_or(at)..at];
    // Look at the complete contiguous public rustdoc block, not unrelated implementation text.
    let prefix=&source[..at]; let start=prefix.rfind("\n\n").unwrap_or(0); let docs=format!("{}{}",&prefix[start..],docs);
    assert!(docs.contains("fsmonitor") && docs.contains("dry-run"),"public maintenance docs must explain pending native stop");
}
