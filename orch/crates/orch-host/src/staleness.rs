//! 二进制陈旧自检（r57/B169，H35）：判断当前二进制的构建基线相对 main 是否已过期。
//! 背景：r55 合入 B162 的主仓写保护后，planner 用的是 1.5 小时前的旧二进制——
//! 源码里有 `ensure_main_guard`，二进制里没有，安全机制整个 r56 开轮期从未生效。
//! 判据不能写成 `build_sha == main_sha`：执行者天天在 worktree 里构建（HEAD ≠ main），
//! 且 main 上大量提交是 ledger/BOARD/CURRENT 协调产物——两种简化都会稳定假红。
//! （planner 预置占位：lib.rs 声明先行入库，B169 在本文件内实现，勿动 lib.rs——frozenPaths。）

use std::path::Path;

/// Git commit embedded by the CLI build script.
///
/// A missing value is intentional: source archives, unavailable `git`, and
/// builds outside a repository must remain usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStamp {
    pub commit: Option<String>,
}

/// Result of comparing the embedded build commit with the repository's main
/// branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleVerdict {
    /// No compiled input changed after the build, or the build came from a
    /// worktree branch that is not an ancestor of main.
    Fresh,
    /// The build commit is an ancestor of main and a compiled input changed.
    Stale,
    /// The comparison cannot be made safely (for example, no build stamp or
    /// unavailable git metadata). Callers must degrade gracefully.
    Unknown,
}

/// Diagnostic detail for the runtime guard and `orch doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalenessReport {
    pub verdict: StaleVerdict,
    pub build_sha: Option<String>,
    pub main_sha: Option<String>,
    pub changed_inputs: Vec<String>,
    pub detail: String,
}

/// Pure staleness criterion.
///
/// `build_is_ancestor` is supplied by the IO layer so this function remains
/// deterministic and directly contract-testable. A non-ancestor build is the
/// normal executor-worktree case, not evidence of an obsolete binary.
pub fn staleness_verdict(
    stamp: &BuildStamp,
    main_sha: &str,
    build_is_ancestor: bool,
    changed_paths: &[String],
) -> StaleVerdict {
    let Some(build_sha) = stamp
        .commit
        .as_deref()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
    else {
        return StaleVerdict::Unknown;
    };

    if build_sha == main_sha || !build_is_ancestor {
        return StaleVerdict::Fresh;
    }

    if changed_paths.iter().any(|path| is_compilation_input(path)) {
        StaleVerdict::Stale
    } else {
        StaleVerdict::Fresh
    }
}

/// Inspect a repository without ever turning missing git metadata into a hard
/// failure. State-changing/read-only policy is deliberately left to the CLI.
pub fn inspect_repository(root: &Path, stamp: &BuildStamp) -> StalenessReport {
    let build_sha = stamp
        .commit
        .as_deref()
        .map(str::trim)
        .filter(|sha| !sha.is_empty())
        .map(str::to_owned);
    let Some(build_sha) = build_sha else {
        return unknown_report(
            None,
            None,
            "构建印记缺失（可能在 git 外构建）；陈旧自检降级放行",
        );
    };

    let main_sha = match crate::gitx::rev_parse(root, "main") {
        Ok(sha) => sha,
        Err(error) => {
            return unknown_report(
                Some(build_sha),
                None,
                format!("无法读取 main SHA（git/仓库不可用）：{error:#}；陈旧自检降级放行"),
            )
        }
    };

    if build_sha == main_sha {
        return StalenessReport {
            verdict: StaleVerdict::Fresh,
            build_sha: Some(build_sha),
            main_sha: Some(main_sha),
            changed_inputs: Vec::new(),
            detail: "构建提交等于 main 尖端".into(),
        };
    }

    let build_is_ancestor = match crate::gitx::is_ancestor(root, &build_sha, &main_sha) {
        Ok(value) => value,
        Err(error) => {
            return unknown_report(
                Some(build_sha),
                Some(main_sha),
                format!("无法判定构建提交与 main 的祖先关系：{error:#}；陈旧自检降级放行"),
            )
        }
    };
    if !build_is_ancestor {
        return StalenessReport {
            verdict: StaleVerdict::Fresh,
            build_sha: Some(build_sha),
            main_sha: Some(main_sha),
            changed_inputs: Vec::new(),
            detail: "构建提交不是 main 的祖先（正常 worktree/分叉构建），不判陈旧".into(),
        };
    }

    let changed_paths = match crate::gitx::diff_names(root, &build_sha, &main_sha) {
        Ok(paths) => paths,
        Err(error) => {
            return unknown_report(
                Some(build_sha),
                Some(main_sha),
                format!("无法读取 build..main 改动文件：{error:#}；陈旧自检降级放行"),
            )
        }
    };
    let changed_inputs = changed_paths
        .iter()
        .filter(|path| is_compilation_input(path))
        .cloned()
        .collect::<Vec<_>>();
    let verdict = staleness_verdict(stamp, &main_sha, true, &changed_paths);
    let detail = if verdict == StaleVerdict::Stale {
        format!(
            "build..main 有 {} 个编译输入变更：{}",
            changed_inputs.len(),
            summarize_paths(&changed_inputs)
        )
    } else {
        "build..main 仅含账本、文档或其他非编译输入变更".into()
    };

    StalenessReport {
        verdict,
        build_sha: Some(build_sha),
        main_sha: Some(main_sha),
        changed_inputs,
        detail,
    }
}

fn is_compilation_input(path: &str) -> bool {
    path.starts_with("orch/") || path == ".githooks/reference-transaction"
}

fn unknown_report(
    build_sha: Option<String>,
    main_sha: Option<String>,
    detail: impl Into<String>,
) -> StalenessReport {
    StalenessReport {
        verdict: StaleVerdict::Unknown,
        build_sha,
        main_sha,
        changed_inputs: Vec::new(),
        detail: detail.into(),
    }
}

fn summarize_paths(paths: &[String]) -> String {
    const LIMIT: usize = 4;
    let mut summary = paths
        .iter()
        .take(LIMIT)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > LIMIT {
        summary.push_str(&format!("（另 {} 个）", paths.len() - LIMIT));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(sha: &str) -> BuildStamp {
        BuildStamp {
            commit: Some(sha.into()),
        }
    }

    #[test]
    fn path_matching_is_component_safe() {
        let paths = vec![
            "orchard/readme.md".to_string(),
            ".githooks/reference-transaction.md".to_string(),
        ];
        assert_eq!(
            staleness_verdict(&stamp("a"), "b", true, &paths),
            StaleVerdict::Fresh
        );
    }

    #[test]
    fn missing_stamp_short_circuits_repository_io() {
        let report = inspect_repository(
            Path::new("/path/that/does/not/exist"),
            &BuildStamp { commit: None },
        );
        assert_eq!(report.verdict, StaleVerdict::Unknown);
        assert!(report.detail.contains("构建印记缺失"));
    }
}
