//! B222 seeded-red contract: 临时路径卫生判据必须对**真实代码树**执行（H102），
//! 且 scratch 落点要有回收，不是只有落点。
//!
//! Expected red: compile. `orch_host::mech::scan_repo_temp_path_hygiene` 与
//! `orch_host::util::sweep_test_scratch_root` 在 B222 之前不存在。
//!
//! 本卡的由来：B108 造了 `mech::temp_path_hygiene` 这个判据，但它**只被合成字符串喂过**，
//! 没有任何扫真实代码树的消费者；B137 手工迁移了 7 个文件，随后 B177/B180/B191 三张卡
//! 又把同一个缺陷写了回去（其中 B180 那处在 r62 成为全量门的实际不稳定源 = H92）。
//! 判据有、迁移做过、但没有执行者 —— 所以持续回归。
//!
//! Negative mutations that must turn the named case red:
//! M1. 扫描器只看白名单文件（照抄 test_tmp_hygiene 的硬编码 MIGRATED 表）
//!     -> `scan_covers_every_rust_source_under_crates` 红：新增文件必须自动进入覆盖面。
//! M2. 沿用整文件子串粒度 -> `discussing_the_defect_is_not_committing_it` 红：
//!     谈论该缺陷的文件（守卫自身、注释、本种子）会被误判成缺陷。
//! M3. 把 ULID 之类的强唯一命名也报成 finding -> `collision_free_naming_is_not_flagged` 红。
//! M4. sweep 删掉仍在使用中的 scratch，或对不存在的根报错
//!     -> `sweep_removes_only_expired_entries` 红。
//! M5. 判据认不出「系统临时目录 + pid + 时间戳、无计数器」这个缺陷形状
//!     -> `a_file_with_the_defect_shape_is_reported_with_file_line_and_evidence` 红。

use std::fs;
use std::path::Path;

use orch_host::mech::scan_repo_temp_path_hygiene;
use orch_host::util::{sweep_test_scratch_root, test_scratch_dir};

fn crates_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("CARGO_MANIFEST_DIR 上溯一级应为 crates/")
        .to_path_buf()
}

#[test]
fn scan_covers_every_rust_source_under_crates() {
    // 覆盖面必须由目录遍历决定，不能由硬编码白名单决定 —— 否则新文件天生免检，
    // 这正是 B177/B180/B191 三次回归的通道。
    let report = scan_repo_temp_path_hygiene(&crates_root()).expect("扫描真实代码树失败");
    assert!(
        report.scanned_files >= 100,
        "覆盖面过小，疑似白名单而非遍历：scanned={}",
        report.scanned_files
    );
    let scanned_self = report
        .scanned_paths
        .iter()
        .any(|p| p.ends_with("tests/scratch_hygiene_enforced.rs"));
    assert!(scanned_self, "本种子文件自身也必须在扫描覆盖面内");
}

#[test]
fn discussing_the_defect_is_not_committing_it() {
    // 整文件子串粒度会把「谈论该缺陷的文件」判成缺陷本身：
    // 实测 test_tmp_hygiene.rs 因注释提到该 API 而被命中，
    // 修 H92 时写在被修函数上方的解释性注释也当场把守卫打红。
    // 判据必须细到能区分「构造临时路径的代码」与「提到它的文字」。
    let report = scan_repo_temp_path_hygiene(&crates_root()).expect("扫描真实代码树失败");
    for noisy in [
        "tests/test_tmp_hygiene.rs",
        "tests/temp_path_hygiene.rs",
        "tests/scratch_hygiene_enforced.rs",
        "src/mech.rs",
        "src/util.rs",
    ] {
        assert!(
            !report.findings.iter().any(|f| f.path.ends_with(noisy)),
            "{noisy} 只是谈论该缺陷，不得被判为缺陷；证据: {:?}",
            report.findings.iter().find(|f| f.path.ends_with(noisy))
        );
    }
}

#[test]
fn collision_free_naming_is_not_flagged() {
    // budget.rs 用 ULID 命名临时根，是安全的；整文件粒度会因该文件别处出现
    // process::id() 与时间标记而误报。
    let report = scan_repo_temp_path_hygiene(&crates_root()).expect("扫描真实代码树失败");
    assert!(
        !report.findings.iter().any(|f| f.path.ends_with("src/budget.rs")),
        "ULID 命名不构成撞名风险，不得报 finding"
    );
}

#[test]
fn a_file_with_the_defect_shape_is_reported_with_file_line_and_evidence() {
    // 反向：真缺陷必须报出来，并带可定位的证据（文件 + 行 + 片段）。
    //
    // **刻意不点名仓内某个当前有缺陷的文件**：那是暂态事实，本卡与后续卡就是要消灭它，
    // 断言它「必须仍被报出」会与交付契约机械互斥——r63/B222-A0001 即因此判 BLOCKED，
    // 教训见 coordination/archive/PLANNER-HANDOFF-history.md §2 裁定⑫。冻结契约只能断言交付后永远为真的不变量，
    // 所以这里用测试自建的夹具证明「扫描器认得缺陷形状」。
    //
    // 注意本用例与 `discussing_the_defect_is_not_committing_it` 是一对：
    // 那条要求**本文件自身**（它把缺陷形状写在字符串字面量里）不得被判为缺陷，
    // 这条要求**夹具文件**（同样的形状，但是真的构造临时路径）必须被判为缺陷。
    // 两者合起来精确定义了本卡要求的粒度：区分「谈论」与「构造」。
    let fixture = test_scratch_dir("b222-offender-fixture");
    let crate_dir = fixture.join("probe-crate/tests");
    fs::create_dir_all(&crate_dir).expect("建夹具目录失败");
    let bad = crate_dir.join("bad_scratch_naming.rs");
    let sample = r#"
fn temp_root(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("orch-probe-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    root
}
"#;
    fs::write(&bad, sample).expect("写夹具失败");

    let report = scan_repo_temp_path_hygiene(&fixture).expect("扫描夹具失败");
    let hit = report
        .findings
        .iter()
        .find(|f| f.path.ends_with("bad_scratch_naming.rs"))
        .expect("系统临时目录 + pid + 时间戳、无计数器的命名必须被报出");
    assert!(hit.line > 0, "finding 必须给出行号，便于直接定位");
    assert!(
        !hit.evidence.trim().is_empty(),
        "finding 必须带源码证据片段"
    );
    let _ = fs::remove_dir_all(&fixture);
}

#[test]
fn sweep_removes_only_expired_entries() {
    // ⚠️ **私有根，绝不扫共享的 target/test-tmp**。
    // A0002 实证：上一版种子把 `test_scratch_dir(...)` 的**父目录**（= 全套件共享的
    // scratch 根）连同 ttl=0 一起交给 sweep，于是在全量门并发时删掉了其他测试正在用的
    // scratch，随机打红与本卡毫无关系的用例（两次收取分别打红
    // `fault_injection::managed_process_group_uses_term_grace_kill_then_kill0_verification`
    // 与 `wake::tests::b191_ten_nonterminal_children_hit_the_monotonic_deadline_and_reap`）。
    // 契约不变，但**必须在自建的私有根上验证**：治理动作自己绝不能成为新的失败源。
    let root = test_scratch_dir("b222-sweep-root");
    let live = root.join("live");
    let stale = root.join("stale");
    fs::create_dir_all(&live).expect("建 live 失败");
    fs::create_dir_all(&stale).expect("建 stale 失败");
    fs::write(stale.join("marker"), b"x").expect("写 marker 失败");

    let removed = sweep_test_scratch_root(&root, std::time::Duration::from_secs(0), &[live.as_path()])
        .expect("sweep 失败");
    assert!(live.is_dir(), "keep 列表内的 scratch 绝不能被扫掉");
    assert!(!stale.exists(), "过期 scratch 必须被回收");
    assert!(
        removed.iter().any(|p| p.ends_with("stale")),
        "回收清单必须逐项可核对，不能只给个计数；实际: {removed:?}"
    );

    // 幂等：重复 sweep 必须成功且不再删 keep 项。
    let again = sweep_test_scratch_root(&root, std::time::Duration::from_secs(0), &[live.as_path()])
        .expect("重复 sweep 必须幂等成功");
    assert!(!again.iter().any(|p| p.ends_with("live")));

    // 根不存在时友好返回空清单，而不是报错——治理动作不得自己变成新的失败源。
    let missing = root.join("definitely-not-here");
    assert!(
        sweep_test_scratch_root(&missing, std::time::Duration::from_secs(0), &[])
            .expect("不存在的根必须友好返回")
            .is_empty()
    );

    let _ = fs::remove_dir_all(&root);
}
