//! Parallel 预设首切片（r27/B49）：writeSet 互斥分波纯函数 plan_waves。
//! 背景：v1 relay 串行;r26 实战证明 writeSet 两两互斥的棒可安全并行(planner 手工分波)。
//! 本切片把「分波」机器化;glob 语义留后续切片(首切片=字符串精确交集,函数头注释披露)。
//! （planner 预置占位：lib.rs 声明先行入库，B49 在本文件内实现，勿动 lib.rs——frozenPaths。）

/// 按输入顺序把任务规划成可并行执行的波次。
///
/// 与任一较早任务冲突的任务，必须排在这些冲突任务所在最晚波的下一波；完全独立的任务
/// 可以回填到第一波。这样既保持冲突任务的先后关系，也让无冲突任务尽早并行。
///
/// writeSet 交集按 glob/目录前缀感知判断（`dir/**` 覆盖 `dir/` 下任意深度路径）。
pub fn plan_waves(tasks: &[(String, Vec<String>)]) -> Vec<Vec<String>> {
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut placed: Vec<(usize, &[String])> = Vec::new();

    for (task_id, write_set) in tasks {
        let wave_index = placed
            .iter()
            .filter(|(_, prior_write_set)| write_sets_overlap_glob(write_set, prior_write_set))
            .map(|(prior_wave, _)| prior_wave + 1)
            .max()
            .unwrap_or(0);

        if wave_index == waves.len() {
            waves.push(Vec::new());
        }
        waves[wave_index].push(task_id.clone());
        placed.push((wave_index, write_set));
    }

    waves
}

/// Glob/前缀感知的 writeSet 交集原语。
///
/// 除精确相等外，`dir/**` 与 `dir/` 下任意深度的路径冲突；判定左右对称。
/// 本原语不改变 `plan_waves` 的首切片精确字符串语义。
pub fn write_sets_overlap_glob(left: &[String], right: &[String]) -> bool {
    left.iter().any(|left_path| {
        right
            .iter()
            .any(|right_path| path_pair_conflicts(left_path, right_path))
    })
}

fn path_pair_conflicts(left: &str, right: &str) -> bool {
    left == right || recursive_glob_covers(left, right) || recursive_glob_covers(right, left)
}

fn recursive_glob_covers(glob: &str, candidate: &str) -> bool {
    let Some(directory) = glob.strip_suffix("/**") else {
        return false;
    };
    candidate
        .strip_prefix(directory)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Committee 预设首切片（r29/B54）：委员会扇出计划。
/// members = agents 去重后按首次出现顺序保留；quorum 必须落在 1..=members.len() 内。
#[derive(Debug, PartialEq)]
pub struct CommitteePlan {
    pub members: Vec<String>,
    pub quorum: usize,
}

/// 委员会裁决结果。
#[derive(Debug, PartialEq)]
pub enum CommitteeOutcome {
    Approved,
    Rejected,
}

/// 规划委员会：agents 去重保序 + quorum 范围校验。
/// quorum 必须落在 1..=members.len()（去重后成员数）；否则 Err（文案含实际 quorum 与允许上界）。
pub fn plan_committee(agents: &[String], quorum: usize) -> Result<CommitteePlan, String> {
    let mut members: Vec<String> = Vec::new();
    for a in agents {
        if !members.contains(a) {
            members.push(a.clone());
        }
    }
    let max = members.len();
    if quorum < 1 || quorum > max {
        return Err(format!(
            "quorum {quorum} 越界：合法范围 1..={max}（去重后成员数）"
        ));
    }
    Ok(CommitteePlan { members, quorum })
}

/// 法定票数裁决：赞成（true）票数 >= quorum ⇒ Approved；否则 Rejected。
/// 阈值是「达到即通过」（>=，不是 >）。
pub fn committee_verdict(votes: &[bool], quorum: usize) -> CommitteeOutcome {
    let yeas = votes.iter().filter(|&&v| v).count();
    if yeas >= quorum {
        CommitteeOutcome::Approved
    } else {
        CommitteeOutcome::Rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn task(id: &str, write_set: &[&str]) -> (String, Vec<String>) {
        (
            id.to_string(),
            write_set.iter().map(|path| path.to_string()).collect(),
        )
    }

    #[test]
    fn recursive_glob_expands_but_bare_dir_stays_independent() {
        let tasks = [
            task("glob", &["src/**"]),
            task("file", &["src/main.rs"]),
            task("directory", &["src"]),
        ];

        assert_eq!(
            plan_waves(&tasks),
            vec![
                vec!["glob".to_string(), "directory".to_string()],
                vec!["file".to_string()],
            ]
        );
    }

    #[test]
    fn every_task_appears_once_and_each_wave_keeps_input_order() {
        let tasks = [
            task("A", &["a.rs"]),
            task("B", &["a.rs", "b.rs"]),
            task("C", &["b.rs"]),
            task("D", &["d.rs"]),
            task("E", &["a.rs"]),
        ];
        let positions: HashMap<&str, usize> = tasks
            .iter()
            .enumerate()
            .map(|(index, (task_id, _))| (task_id.as_str(), index))
            .collect();

        let waves = plan_waves(&tasks);
        let emitted: Vec<&str> = waves
            .iter()
            .flat_map(|wave| wave.iter().map(String::as_str))
            .collect();
        let unique: HashSet<&str> = emitted.iter().copied().collect();

        assert_eq!(emitted.len(), tasks.len());
        assert_eq!(unique.len(), tasks.len());
        assert!(waves.iter().all(|wave| wave
            .windows(2)
            .all(|pair| { positions[pair[0].as_str()] < positions[pair[1].as_str()] })));
    }

    #[test]
    fn recursive_glob_does_not_cover_the_directory_itself() {
        assert!(!write_sets_overlap_glob(
            &["src/**".to_string()],
            &["src".to_string()]
        ));
    }

    #[test]
    fn only_terminal_recursive_glob_has_pattern_semantics() {
        assert!(!write_sets_overlap_glob(
            &["src/*.rs".to_string(), "src/**/mod.rs".to_string()],
            &["src/main.rs".to_string(), "src/deep/mod.rs".to_string()]
        ));
    }
}
