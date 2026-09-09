//! B160 seeded-red contract (H28: the review site is provisioned, not assumed).
//!
//! Negative mutations that must turn the named case red:
//! M1. Put the review worktree (or its build dir) anywhere outside the repo
//!     root. That is precisely what killed r53: opencode's sandbox
//!     auto-rejects external directories, so a /tmp worktree is unreadable and
//!     the main checkout becomes the only place it can write.
//! M2. Treat a declared sandbox range as fact instead of probing it. "Declared"
//!     is not "reachable"; six sessions died silently on that assumption.
//! M3. Let the injection go out when provisioning failed — a review request
//!     whose site does not exist is the silent-failure shape all over again.

use orch_host::agent_profile::{sandbox_writable_roots, SandboxScope};
use orch_host::wake::{review_site_plan, ReviewSite};

fn site(task: &str, role: &str, agent: &str) -> Result<ReviewSite, String> {
    review_site_plan("/repo/root", task, role, agent)
}

#[test]
fn the_review_site_lives_inside_the_repo() {
    // M1: both paths must be repo-relative. /tmp is the failure this card
    // exists to remove.
    let plan = site("B900", "primary", "executor-claw").expect("plan is representable");
    assert!(
        plan.worktree.starts_with("/repo/root/"),
        "worktree must be inside the repo: {}",
        plan.worktree
    );
    assert!(
        plan.target_dir.starts_with("/repo/root/"),
        "build dir must be inside the repo: {}",
        plan.target_dir
    );
    for banned in ["/tmp", "/private/tmp", "/var/folders"] {
        assert!(!plan.worktree.starts_with(banned));
        assert!(!plan.target_dir.starts_with(banned));
    }
    // Identity-scoped so two reviewers of the same attempt never collide.
    let other = site("B900", "secondary", "executor-opencode").expect("plan");
    assert_ne!(plan.worktree, other.worktree);
    assert_ne!(plan.target_dir, other.target_dir);
    // Same identity twice is the same site — provisioning must be idempotent.
    assert_eq!(plan.worktree, site("B900", "primary", "executor-claw").unwrap().worktree);
}

#[test]
fn an_undeclared_sandbox_defaults_to_repo_root_only() {
    // M2: the safe default is the narrow one. An agent that says nothing is
    // assumed to reach only the repo — which is exactly what M1 provisions.
    assert_eq!(sandbox_writable_roots(None), SandboxScope::RepoRootOnly);
    assert_eq!(
        sandbox_writable_roots(Some(&[])),
        SandboxScope::RepoRootOnly,
        "an empty declaration is not a wide-open declaration"
    );
    let wide = sandbox_writable_roots(Some(&["/tmp".to_string()]));
    assert_ne!(wide, SandboxScope::RepoRootOnly);
    // Declaring a range never substitutes for probing it.
    assert!(
        !wide.is_verified(),
        "a declared scope starts unverified; only a probe may mark it reachable"
    );
}

#[test]
fn provisioning_failure_must_block_the_injection() {
    // M3: "site ready" is part of the same production action as "request
    // issued". A request whose site is missing reproduces the silent failure.
    let err = review_site_plan("/repo/root", "B900", "unknown-role", "executor-claw")
        .unwrap_err();
    assert!(
        err.contains("role") || err.contains("unknown-role"),
        "an unusable identity must be refused by name: {err}"
    );
    assert!(review_site_plan("", "B900", "primary", "executor-claw").is_err());
    assert!(review_site_plan("/repo/root", "", "primary", "executor-claw").is_err());
}
