//! r85 B332: real schema-3 role must be accepted without reclaiming an active target.
use std::{fs,time::Duration};
#[test]
fn schema3_review_lease_is_accepted_and_active_cache_is_preserved() {
 let root=orch_host::util::test_scratch_dir("b332-review-role");
 let target="orch/target/review-B330-A0001-review-claude-g01";
 fs::create_dir_all(root.join(target)).unwrap();
 fs::write(root.join(target).join("sentinel"),b"keep active").unwrap();
 let event=serde_json::from_value(serde_json::json!({"eventId":"b332-lease","ts":"2026-09-07T00:00:00Z","actor":"runtime:orch","type":"WorkspaceLeased","taskId":"B330","round":"r84","payload":{"agent":"claude","attemptId":"B330-A0001","generation":1,"paths":{"target":target,"worktree":".worktrees/review-B330-A0001-review-claude-g01"},"reviewedHead":"8f1f1c49b790a4e3d5a79dd7f2ee5fbe156d3d18","role":"review","siteId":"B330-review-claude-g01","wakeId":"wake-b332"}})).unwrap();
 let report=orch_host::buildcache::sweep_targets(&root,&[event],Duration::ZERO).expect("valid review role must not be misreported as missing");
 assert!(report.removed.is_empty());
 assert_eq!(fs::read(root.join(target).join("sentinel")).unwrap(),b"keep active");
}
