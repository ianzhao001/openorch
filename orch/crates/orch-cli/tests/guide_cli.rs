//! Portable AI mechanical-guide CLI contract.
//!
//! The copied executable is launched from an otherwise empty directory so this
//! proves the guide is embedded in the product rather than read from the source
//! checkout or from a project's coordination state.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "orch-guide-cli-{label}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create guide CLI fixture");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copied_orch(root: &Path) -> PathBuf {
    let destination = root.join("orch-standalone");
    fs::copy(support::orch_bin(), &destination).expect("copy Cargo-built orch binary");
    destination
}

fn fixture_orch_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    support::configure_fixture_git_env(&mut command, &[]);
    command
}

fn entries(root: &Path) -> Vec<String> {
    let mut names = fs::read_dir(root)
        .expect("read guide fixture")
        .map(|entry| {
            entry
                .expect("read guide fixture entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn copied_binary_prints_embedded_guide_without_coordination_or_writes() {
    let root = TestRoot::new("standalone");
    let binary = copied_orch(root.path());
    let before = entries(root.path());

    let output = fixture_orch_command(&binary)
        .arg("guide")
        .current_dir(root.path())
        .output()
        .expect("run standalone orch guide");

    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("guide output is UTF-8");
    assert!(
        output.stderr.is_empty(),
        "standalone guide must not probe or warn about repository state: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("# orch AI 机械契约指南"), "{stdout}");
    assert!(stdout.contains("ReportCollectExecuting"), "{stdout}");
    assert!(stdout.contains("`nudge` 不是租约释放命令"), "{stdout}");
    assert!(!root.path().join("coordination").exists());
    assert_eq!(
        entries(root.path()),
        before,
        "orch guide must not write files"
    );
}

#[test]
fn guide_section_and_check_are_standalone_and_fail_closed() {
    let root = TestRoot::new("section-check");
    let binary = copied_orch(root.path());

    let section = fixture_orch_command(&binary)
        .args(["guide", "--section", "leases"])
        .current_dir(root.path())
        .output()
        .expect("run guide section");
    assert!(section.status.success());
    let section = String::from_utf8(section.stdout).unwrap();
    assert!(section.contains("## Durable action 与租约"), "{section}");
    assert!(section.contains("outcomeUnknown"), "{section}");
    assert!(!section.contains("## 命令总表"), "{section}");

    let check = fixture_orch_command(&binary)
        .args(["guide", "--check"])
        .current_dir(root.path())
        .output()
        .expect("run guide check");
    assert!(
        check.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stdout).contains("orch guide: OK"));
    assert!(String::from_utf8_lossy(&check.stdout).contains("wakeActions=4"));

    let recovery = fixture_orch_command(&binary)
        .args(["guide", "--section", "recovery"])
        .current_dir(root.path())
        .output()
        .expect("run recovery guide section");
    assert!(recovery.status.success());
    let recovery = String::from_utf8(recovery.stdout).unwrap();
    assert!(recovery.contains("collect 持有者被 kill"), "{recovery}");
    assert!(
        recovery.contains("BLOCKED attempt 经 nudge 交接"),
        "{recovery}"
    );
    assert!(recovery.contains("outcomeUnknown: true"), "{recovery}");

    let unknown = fixture_orch_command(&binary)
        .args(["guide", "--section", "not-a-section"])
        .current_dir(root.path())
        .output()
        .expect("run unknown guide section");
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("未知 guide section"));
}

#[test]
fn init_creates_and_backfills_operator_runbook_without_overwrite() {
    let root = TestRoot::new("init-backfill");
    let binary = copied_orch(root.path());

    let first = fixture_orch_command(&binary)
        .args(["--root", root.path().to_str().unwrap(), "init"])
        .current_dir(root.path())
        .output()
        .expect("run first orch init");
    assert!(
        first.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let runbook = root.path().join("coordination/AI-OPERATOR-RUNBOOK.md");
    assert!(runbook.is_file());

    // A newly added product scaffold is backfilled by re-running `init`; the
    // command stays create-missing/idempotent.
    fs::remove_file(&runbook).unwrap();

    let backfill = fixture_orch_command(&binary)
        .args(["--root", root.path().to_str().unwrap(), "init"])
        .current_dir(root.path())
        .output()
        .expect("run backfill init");
    assert!(
        backfill.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&backfill.stdout),
        String::from_utf8_lossy(&backfill.stderr)
    );
    assert!(fs::read_to_string(&runbook)
        .unwrap()
        .contains("机械契约优先"));

    let customized = "# project-owned policy\nnever overwrite me\n";
    fs::write(&runbook, customized).unwrap();

    let preserve = fixture_orch_command(&binary)
        .args(["--root", root.path().to_str().unwrap(), "init"])
        .current_dir(root.path())
        .output()
        .expect("run preserve init");
    assert!(
        preserve.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&preserve.stdout),
        String::from_utf8_lossy(&preserve.stderr)
    );
    assert_eq!(fs::read_to_string(&runbook).unwrap(), customized);

    // Backfill convenience must not cost the bootstrap guard: once a round is
    // live, `orch init` stays fail-closed. Scaffolds added mid-round are
    // committed by the planner, not backfilled by re-running bootstrap.
    fs::write(
        root.path().join("coordination/runtime/CURRENT-ROUND"),
        "running-round\n",
    )
    .unwrap();
    let refused = fixture_orch_command(&binary)
        .args(["--root", root.path().to_str().unwrap(), "init"])
        .current_dir(root.path())
        .output()
        .expect("run active-round init");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("已有 CURRENT-ROUND 时拒绝 orch init"),
        "stderr={}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(fs::read_to_string(&runbook).unwrap(), customized);
}
