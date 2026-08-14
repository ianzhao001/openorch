use std::fs;
use std::path::{Path, PathBuf};

use orch_host::card::{Card, CardMeta};
use orch_host::oracle::{copy_seeds_contained, validate_seed_paths};

struct Site {
    _scratch: PathBuf,
    root: PathBuf,
    worktree: PathBuf,
    outside: PathBuf,
}

impl Site {
    fn new(tag: &str) -> Self {
        let scratch = orch_host::util::test_scratch_dir(&format!("seed-containment-{tag}"));
        let root = scratch.join("repo");
        let worktree = scratch.join("worktree");
        let outside = scratch.join("outside");
        fs::create_dir_all(root.join("seeds")).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&outside).unwrap();
        Self {
            _scratch: scratch,
            root,
            worktree,
            outside,
        }
    }

    fn write_seed(&self, bytes: &[u8]) {
        fs::write(self.root.join("seeds/contract.rs"), bytes).unwrap();
    }
}

fn card(src: &str, target: &str, write_set: &[&str], frozen_paths: &[&str]) -> Card {
    let meta: CardMeta = serde_json::from_value(serde_json::json!({
        "taskId": "T1",
        "seeds": [{"src": src, "target": target}],
        "writeSet": write_set,
        "frozenPaths": frozen_paths,
        "gates": {"fast": []}
    }))
    .unwrap();
    Card {
        meta,
        body: String::new(),
        rel_path: "coordination/rounds/r-test/tasks/T1.md".into(),
    }
}

fn assert_rejected_without_touching_victim(site: &Site, candidate: Card, victim: &Path) {
    let before = fs::read(victim).unwrap();
    let error = copy_seeds_contained(&site.root, &site.worktree, &candidate).unwrap_err();
    assert_eq!(
        fs::read(victim).unwrap(),
        before,
        "拒绝后 victim 被改写: {error:#}"
    );
}

#[test]
fn copies_exact_bytes_inside_worktree_and_can_replace_regular_file() {
    let site = Site::new("happy");
    let bytes = b"#[test]\nfn contract() {}\n";
    site.write_seed(bytes);
    fs::create_dir_all(site.worktree.join("tests")).unwrap();
    fs::write(site.worktree.join("tests/contract.rs"), b"old\n").unwrap();
    let task = card(
        "seeds/contract.rs",
        "tests/contract.rs",
        &["tests/contract.rs"],
        &[],
    );

    validate_seed_paths(&site.root, &task).unwrap();
    copy_seeds_contained(&site.root, &site.worktree, &task).unwrap();

    assert_eq!(
        fs::read(site.worktree.join("tests/contract.rs")).unwrap(),
        bytes
    );
}

#[test]
fn rejects_parent_and_absolute_source_paths() {
    let site = Site::new("source-escape");
    let victim = site.outside.join("victim.rs");
    fs::write(&victim, b"outside\n").unwrap();

    for src in [
        "../outside/victim.rs".to_string(),
        victim.to_string_lossy().into_owned(),
    ] {
        let task = card(&src, "tests/contract.rs", &["tests/contract.rs"], &[]);
        assert_rejected_without_touching_victim(&site, task, &victim);
    }
    assert!(!site.worktree.join("tests/contract.rs").exists());
}

#[test]
fn rejects_parent_and_absolute_target_paths() {
    let site = Site::new("target-escape");
    site.write_seed(b"seed\n");
    let victim = site.outside.join("victim.rs");
    fs::write(&victim, b"outside\n").unwrap();

    let parent = card(
        "seeds/contract.rs",
        "../outside/victim.rs",
        &["../outside/victim.rs"],
        &[],
    );
    assert_rejected_without_touching_victim(&site, parent, &victim);

    let absolute_target = victim.to_string_lossy().into_owned();
    let absolute = card(
        "seeds/contract.rs",
        &absolute_target,
        &[&absolute_target],
        &[],
    );
    assert_rejected_without_touching_victim(&site, absolute, &victim);
}

#[cfg(unix)]
#[test]
fn rejects_source_file_and_parent_symlinks() {
    use std::os::unix::fs::symlink;

    let site = Site::new("source-symlink");
    let real = site.outside.join("real.rs");
    fs::write(&real, b"outside\n").unwrap();
    symlink(&real, site.root.join("seeds/file-link.rs")).unwrap();
    symlink(&site.outside, site.root.join("parent-link")).unwrap();

    for src in ["seeds/file-link.rs", "parent-link/real.rs"] {
        let task = card(src, "tests/contract.rs", &["tests/contract.rs"], &[]);
        let error = validate_seed_paths(&site.root, &task).unwrap_err();
        assert!(
            error.to_string().contains("符号链接"),
            "unexpected error: {error:#}"
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_target_parent_symlink_without_modifying_its_destination() {
    use std::os::unix::fs::symlink;

    let site = Site::new("target-symlink");
    site.write_seed(b"seed\n");
    let victim = site.outside.join("contract.rs");
    fs::write(&victim, b"outside\n").unwrap();
    symlink(&site.outside, site.worktree.join("tests")).unwrap();
    let task = card(
        "seeds/contract.rs",
        "tests/contract.rs",
        &["tests/contract.rs"],
        &[],
    );

    assert_rejected_without_touching_victim(&site, task, &victim);
}

#[test]
fn rejects_targets_outside_write_set_or_inside_frozen_paths() {
    let site = Site::new("card-domain");
    site.write_seed(b"seed\n");

    let outside_write_set = card("seeds/contract.rs", "tests/contract.rs", &["src/**"], &[]);
    assert!(validate_seed_paths(&site.root, &outside_write_set)
        .unwrap_err()
        .to_string()
        .contains("writeSet"));

    let frozen = card(
        "seeds/contract.rs",
        "tests/contract.rs",
        &["tests/**"],
        &["tests/**"],
    );
    assert!(validate_seed_paths(&site.root, &frozen)
        .unwrap_err()
        .to_string()
        .contains("frozenPaths"));
}

#[test]
fn rejects_duplicate_targets_before_copying_any_seed() {
    let site = Site::new("duplicates");
    site.write_seed(b"first\n");
    fs::write(site.root.join("seeds/second.rs"), b"second\n").unwrap();
    let meta: CardMeta = serde_json::from_value(serde_json::json!({
        "taskId": "T1",
        "seeds": [
            {"src": "seeds/contract.rs", "target": "tests/contract.rs"},
            {"src": "seeds/second.rs", "target": "tests/contract.rs"}
        ],
        "writeSet": ["tests/contract.rs"]
    }))
    .unwrap();
    let task = Card {
        meta,
        body: String::new(),
        rel_path: String::new(),
    };

    assert!(copy_seeds_contained(&site.root, &site.worktree, &task).is_err());
    assert!(!site.worktree.join("tests/contract.rs").exists());
}

#[test]
fn rejects_ancestor_descendant_targets_before_copying_any_seed() {
    for targets in [["tests", "tests/contract.rs"], ["tests/contract.rs", "tests"]] {
        let site = Site::new("prefix-collision");
        site.write_seed(b"first\n");
        fs::write(site.root.join("seeds/second.rs"), b"second\n").unwrap();
        let meta: CardMeta = serde_json::from_value(serde_json::json!({
            "taskId": "T1",
            "seeds": [
                {"src": "seeds/contract.rs", "target": targets[0]},
                {"src": "seeds/second.rs", "target": targets[1]}
            ],
            "writeSet": ["tests", "tests/contract.rs"]
        }))
        .unwrap();
        let task = Card {
            meta,
            body: String::new(),
            rel_path: String::new(),
        };

        let error = copy_seeds_contained(&site.root, &site.worktree, &task).unwrap_err();
        assert!(error.to_string().contains("互为祖先"), "{error:#}");
        assert!(!site.worktree.join("tests").exists());
    }
}
