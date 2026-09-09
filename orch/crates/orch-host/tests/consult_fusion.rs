//! B166 seeded-red contract (H30: every member works in its own in-repo site,
//! and one dead member never kills the fusion).
//!
//! Negative mutations that must turn the named case red:
//! M1. Put a consult site anywhere outside the repo root, or let two members of
//!     the same run share a path. /tmp is unreadable to opencode's sandbox (the
//!     r53 incident), and repeated legacy fusion members need stable indices
//!     — colliding paths would silently merge three fusionlists into one.
//! M2. Make any single member's failure fatal, or collapse a timeout into a
//!     generic failure. Partial failure is the designed behaviour; a slow member
//!     must not be able to erase its peers' answers.
//! M3. Spawn before archiving the outgoing bytes, or reorder results away from
//!     the input order. Every byte that leaves the repo must be on disk first,
//!     and self-fusion needs stable indices to tell its three runs apart.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use orch_host::consult::MemberStatus;
use orch_host::fusion::{consult_site_plan, run_fusion, FusionMember};

static TEMP_ROOT_SEQ: AtomicU64 = AtomicU64::new(0);
static FUSION_TEST_LOCK: Mutex<()> = Mutex::new(());

fn temp_root(name: &str) -> PathBuf {
    let orch_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR 上溯两级应为 orch 工作区根");
    let seq = TEMP_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
    orch_root.join("target/test-tmp").join(format!(
        "consult-fusion-{name}-{}-{}-{seq}",
        std::process::id(),
        ulid::Ulid::new()
    ))
}

const CONSULT_ID: &str = "01JCONSULTSEEDFIXTURE0001";

#[test]
fn every_member_gets_its_own_site_inside_the_repo() {
    // M1: repo-internal and index-distinct, so self-fusion cannot collapse.
    let a = consult_site_plan("/repo/root", CONSULT_ID, 0, "consult-codex").expect("site plan");
    let b = consult_site_plan("/repo/root", CONSULT_ID, 1, "consult-codex").expect("site plan");
    for site in [&a, &b] {
        assert!(
            site.worktree.starts_with("/repo/root/"),
            "worktree must live inside the repo: {}",
            site.worktree
        );
        assert!(
            site.target_dir.starts_with("/repo/root/"),
            "build dir must live inside the repo: {}",
            site.target_dir
        );
        for banned in ["/tmp", "/private/tmp", "/var/folders"] {
            assert!(!site.worktree.starts_with(banned));
            assert!(!site.target_dir.starts_with(banned));
        }
    }
    assert_ne!(
        a.worktree, b.worktree,
        "same adapter, different index, different site"
    );
    assert_ne!(a.target_dir, b.target_dir);
    // Idempotent: the same identity always plans to the same place.
    let again = consult_site_plan("/repo/root", CONSULT_ID, 0, "consult-codex").unwrap();
    assert_eq!(a.worktree, again.worktree);
    // Escapes are refused by name.
    assert!(consult_site_plan("", CONSULT_ID, 0, "consult-codex").is_err());
    assert!(consult_site_plan("/repo/root", CONSULT_ID, 0, "../escape").is_err());
}

#[test]
fn one_dead_member_does_not_kill_the_fusion_and_a_slow_one_is_its_own_state() {
    let _serial = FUSION_TEST_LOCK.lock().unwrap();
    // M2: three fake adapters — ok / non-zero exit / sleeps past its deadline.
    let root = temp_root("partial");
    let members = vec![
        FusionMember::new(0, "seed-ok"),
        FusionMember::new(1, "seed-boom"),
        FusionMember::new(2, "seed-slow"),
    ];
    let outcomes = run_fusion(&root, &members, "question", seed_limits())
        .expect("a partially failing fusion still returns");

    assert_eq!(outcomes.len(), 3, "every member gets an outcome slot");
    assert_eq!(outcomes[0].status, MemberStatus::Ok);
    assert!(outcomes[0]
        .answer
        .as_deref()
        .unwrap_or_default()
        .contains("seed-ok"));
    assert_eq!(outcomes[1].status, MemberStatus::Failed);
    assert!(
        outcomes[1].failure_class.is_some(),
        "a failed member carries a structured class, not prose"
    );
    assert_eq!(
        outcomes[2].status,
        MemberStatus::TimedOut,
        "a timeout is its own state, never folded into Failed"
    );
    assert!(
        outcomes.iter().any(|o| o.status == MemberStatus::Ok),
        "one survivor is enough for the fusion to continue"
    );
}

#[test]
fn outgoing_bytes_are_archived_before_the_spawn_and_order_is_stable() {
    let _serial = FUSION_TEST_LOCK.lock().unwrap();
    // M3: the archive is a precondition of the spawn, and indices are stable.
    let root = temp_root("archive");
    let members = vec![
        FusionMember::new(0, "seed-echo-archive"),
        FusionMember::new(1, "seed-ok"),
    ];
    let outcomes = run_fusion(&root, &members, "question", seed_limits()).expect("fusion runs");

    // seed-echo-archive prints back whether its own request archive already
    // existed when it started — the only way to prove ordering from outside.
    assert!(
        outcomes[0]
            .answer
            .as_deref()
            .unwrap_or_default()
            .contains("ARCHIVE_PRESENT=1"),
        "the member's request bytes must be on disk before it is spawned"
    );
    assert_eq!(outcomes[0].index, 0);
    assert_eq!(outcomes[1].index, 1);
    assert_eq!(outcomes[0].member, "seed-echo-archive");
    assert_eq!(outcomes[1].member, "seed-ok");
}

fn seed_limits() -> orch_host::consult::ConsultLimits {
    orch_host::consult::ConsultLimits {
        per_member_timeout_secs: 5,
        total_wall_secs: 30,
        max_members: 8,
    }
}
