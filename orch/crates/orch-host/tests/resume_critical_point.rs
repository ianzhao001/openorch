#[test]
fn resume_rebinds_signed_task_after_before_wake_hook() {
    let source = include_str!("../src/tierf.rs");
    let resume = source.find("pub fn run_resume_with_hook(").unwrap();
    let tail = &source[resume..];
    let hook = tail.find("hook(\"before-wake\")").unwrap();
    let rebind = tail[hook..]
        .find("require_active_tierf_task(root, &round, task_id)")
        .unwrap();
    let launching = tail[hook..].find("\"ResumeWakeLaunching\"").unwrap();
    assert!(rebind < launching);
    assert!(tail[hook..].contains("require_active_tierf_task_from_events"));
    assert!(source.contains("CriticalPointDrift"));
}
