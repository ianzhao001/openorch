//! 红种子契约 · B108 · 临时路径唯一性。
//! 预期红：compile，缺少 unique_scratch_name/temp_path_hygiene。
//! M1：退化为 pid+nanos、无 AtomicU64，hygiene_rejects_pid_plus_time_without_counter 红。
//! M2：计数器不递增，generated_names_are_unique 红。
//! M3：扫描把合规 helper 误报，hygiene_accepts_counter_helper 红。

use std::collections::HashSet;

use orch_host::mech::temp_path_hygiene;
use orch_host::util::unique_scratch_name;

#[test]
fn generated_names_are_unique_and_identify_pid() {
    let names = (0..512)
        .map(|_| unique_scratch_name("gate"))
        .collect::<Vec<_>>();
    let unique = names.iter().collect::<HashSet<_>>();
    assert_eq!(unique.len(), names.len());
    let pid = std::process::id().to_string();
    assert!(names.iter().all(|name| name.contains(&pid)));
}

#[test]
fn hygiene_rejects_pid_plus_time_without_counter() {
    let bad = r#"std::env::temp_dir().join(format!("orch-{}-{}", std::process::id(), nanos))"#;
    let findings = temp_path_hygiene(bad);
    assert!(!findings.is_empty(), "pid+nanos without AtomicU64 must be rejected");
}

#[test]
fn hygiene_accepts_counter_helper() {
    let good = r#"root.join(unique_scratch_name("fixture"))"#;
    assert!(temp_path_hygiene(good).is_empty());
}
