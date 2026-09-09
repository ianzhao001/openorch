use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn contract_loaded() {}

const BASELINE: &str = include_str!("../../../../../coordination/source-shape-baseline-v1.json");
const HOST: &str = "orch/crates/orch-host/src/attempt.rs";
const BODY: &str = "orch/crates/orch-host/src/attempt/tests_body.rs";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn package_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

fn baseline_attempt() -> Value {
    serde_json::from_str::<Value>(BASELINE).expect("基线不是合法 JSON")["attempt"].clone()
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[derive(Clone, Copy)]
enum ReaderBase {
    BodyDirectory,
    PackageRoot,
}

#[derive(Clone, Copy)]
enum ReaderTarget {
    RustFile,
    Directory,
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop_normal = matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                );
                if can_pop_normal {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// 只接受调用括号后的直接、单行普通或 raw 字符串字面量。
/// 转义字符串、多行拼接、别名和 helper 间接构造属于卡面披露边界。
fn direct_string_literal(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        return Some(&quoted[..quoted.find('"')?]);
    }

    let raw = rest.strip_prefix('r')?;
    let hashes = raw.bytes().take_while(|byte| *byte == b'#').count();
    let content = raw.get(hashes..)?.strip_prefix('"')?;
    let closing = format!("\"{}", "#".repeat(hashes));
    Some(&content[..content.find(&closing)?])
}

fn direct_string_literals_after<'a>(mut line: &'a str, needle: &str) -> Vec<&'a str> {
    let mut literals = Vec::new();
    while let Some((_, rest)) = line.split_once(needle) {
        if let Some(literal) = direct_string_literal(rest) {
            literals.push(literal);
        }
        line = rest;
    }
    literals
}

fn resolved_production_target(
    literal: &str,
    base: ReaderBase,
    target: ReaderTarget,
) -> Option<PathBuf> {
    let root = repo_root();
    let package = package_root();
    let body_dir = root
        .join(BODY)
        .parent()
        .expect("BODY 必须有父目录")
        .to_path_buf();
    let literal = Path::new(literal);
    let candidate = if literal.is_absolute() {
        literal.to_path_buf()
    } else {
        match base {
            ReaderBase::BodyDirectory => body_dir.join(literal),
            ReaderBase::PackageRoot => package.join(literal),
        }
    };
    let candidate = normalize_lexically(&candidate);
    let production_root = normalize_lexically(&package.join("src"));
    if !candidate.starts_with(&production_root) {
        return None;
    }

    match target {
        ReaderTarget::RustFile
            if candidate.extension().and_then(|value| value.to_str()) == Some("rs") =>
        {
            Some(candidate)
        }
        ReaderTarget::Directory => Some(candidate),
        ReaderTarget::RustFile => None,
    }
}

fn production_source_reader_lines(source: &str) -> Vec<String> {
    const CALLS: [(&str, ReaderBase, ReaderTarget); 6] = [
        (
            "include_str!(",
            ReaderBase::BodyDirectory,
            ReaderTarget::RustFile,
        ),
        (
            "include_bytes!(",
            ReaderBase::BodyDirectory,
            ReaderTarget::RustFile,
        ),
        (
            "fs::read_to_string(",
            ReaderBase::PackageRoot,
            ReaderTarget::RustFile,
        ),
        ("fs::read(", ReaderBase::PackageRoot, ReaderTarget::RustFile),
        (
            "File::open(",
            ReaderBase::PackageRoot,
            ReaderTarget::RustFile,
        ),
        (
            "read_dir(",
            ReaderBase::PackageRoot,
            ReaderTarget::Directory,
        ),
    ];

    let mut edges = Vec::new();
    for line in source.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut calls = CALLS.to_vec();
        if line.contains("OpenOptions") {
            calls.push((".open(", ReaderBase::PackageRoot, ReaderTarget::RustFile));
        }
        'calls: for (needle, base, target) in calls {
            for literal in direct_string_literals_after(line, needle) {
                let Some(resolved) = resolved_production_target(literal, base, target) else {
                    continue;
                };
                edges.push(format!("{} -> {}", line.trim(), resolved.display()));
                break 'calls;
            }
        }
    }
    edges
}

#[test]
fn hardening_direct_source_readers_resolve_lexically() {
    let discriminator_fixtures = [
        r#"const _: &str = include_str!("../wake.rs");"#,
        r##"const _: &str = include_str!(r#"../wake.rs"#);"##,
        r#"let _ = fs::read_to_string("src/wake.rs");"#,
        r#"let _ = fs::read_dir("src");"#,
        r#"let _ = std::fs::OpenOptions::new().read(true).open("src/wake.rs");"#,
        r#"let _ = include_str!("../../tests/not-production.rs"); let _ = include_str!("../wake.rs");"#,
    ];
    for fixture in discriminator_fixtures {
        let fixture_edges = production_source_reader_lines(fixture);
        assert_eq!(
            fixture_edges.len(),
            1,
            "reader scanner 必须拒绝内嵌判别样本 {fixture:?}，实际 {fixture_edges:?}"
        );
    }
    assert!(
        production_source_reader_lines(r#"// include_str!("../wake.rs")"#).is_empty(),
        "行首注释里的调用文本不得形成生产读边"
    );
    #[cfg(unix)]
    assert_eq!(
        normalize_lexically(Path::new("/../../../src")),
        PathBuf::from("/src"),
        "绝对路径越过根目录时必须 clamp 在根"
    );

    let body = read(BODY);
    let edges = production_source_reader_lines(&body);
    assert!(
        edges.is_empty(),
        "tests_body.rs 不得直接读取生产源码，实际 {edges:?}"
    );
}

#[test]
fn hardening_host_has_one_cfg_test_shell() {
    let host = read(HOST);
    let marker_count = host
        .lines()
        .filter(|line| line.trim() == "#[cfg(test)]")
        .count();
    assert_eq!(
        marker_count, 1,
        "attempt.rs 必须且只能有一个精确 #[cfg(test)] 标记"
    );

    let marker_at = if host.starts_with("#[cfg(test)]\n") {
        0
    } else {
        host.find("\n#[cfg(test)]\n")
            .map(|index| index + 1)
            .expect("attempt.rs 必须有列 0 的 #[cfg(test)]")
    };
    let kept: Vec<&str> = host[marker_at..]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        kept,
        vec![
            "#[cfg(test)]",
            "mod tests {",
            "include!(\"attempt/tests_body.rs\");",
            "}",
        ],
        "attempt.rs 测试区必须是唯一 cfg 入口与精确 include 壳，实际 {kept:?}"
    );
}

#[test]
fn hardening_body_matches_recorded_digest() {
    let attempt = baseline_attempt();
    let expected = attempt["relocatableBodySha256"]
        .as_str()
        .expect("基线缺 attempt.relocatableBodySha256");
    let range = attempt["relocatableBodyRange"]
        .as_array()
        .expect("基线缺 attempt.relocatableBodyRange");
    let first = range[0].as_u64().expect("body 起始行不是整数") as usize;
    let last = range[1].as_u64().expect("body 结束行不是整数") as usize;

    let body = read(BODY);
    assert_eq!(
        body.lines().count(),
        last - first + 1,
        "搬迁体行数不再等于基线区间 {first}..={last}"
    );
    assert_eq!(
        sha256_hex(&body),
        expected,
        "tests_body.rs 不再是 planner 基线登记的逐字节搬迁体"
    );
}
