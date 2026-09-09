use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};

/// Compiled contract source, shared by both feature-specific projections.
pub const GUIDE_MARKDOWN: &str = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");

/// Closed list of addressable sections; invalid names fail before rendering.
pub const SECTION_NAMES: [&str; 10] = [
    "scope",
    "truth",
    "lifecycle",
    "commands",
    "leases",
    "nudge-resume",
    "recovery",
    "safety",
    "worked-example",
    "quick-reference",
];

const REQUIRED_INVARIANTS: [&str; 4] = [
    "nudge-not-release-collect",
    "ledger-recover-wal-only",
    "live-collect-lease-not-preemptible",
    "expired-collect-outcome-unknown",
];

/// Exact marker counts validated against this binary, not the workspace feature union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuideStats {
    /// Public leaf commands in the current build.
    pub commands: usize,
    /// Paired addressable sections.
    pub sections: usize,
    /// Required safety invariants.
    pub invariants: usize,
    /// Virtual wake controls matched to the runtime action table.
    pub wake_actions: usize,
    /// Public outcome mappings matched to the code-owned table.
    pub dispositions: usize,
}

/// Render the embedded contract or one named section for the current CLI feature.
pub fn render(section: Option<&str>) -> Result<String> {
    let markdown = active_markdown();
    let Some(section) = section else {
        return Ok(markdown);
    };
    if !SECTION_NAMES.contains(&section) {
        bail!(
            "未知 guide section {section:?}；可用值：{}",
            SECTION_NAMES.join(", ")
        );
    }
    let open = format!("<!-- orch-guide-section:{section} -->");
    let close = format!("<!-- orch-guide-section-end:{section} -->");
    let start = markdown
        .find(&open)
        .with_context(|| format!("guide 缺 section 起点 {section}"))?
        + open.len();
    let tail = &markdown[start..];
    let end = tail
        .find(&close)
        .with_context(|| format!("guide 缺 section 终点 {section}"))?;
    Ok(format!("{}\n", tail[..end].trim_matches('\n')))
}

/// Require bidirectional exact coverage of compiled commands and contract markers.
pub fn validate(public_leaf_commands: &[String], wake_actions: &[&str]) -> Result<GuideStats> {
    let section_markers = marker_counts("orch-guide-section:");
    let section_end_markers = marker_counts("orch-guide-section-end:");
    let expected_sections = SECTION_NAMES
        .iter()
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();
    require_exact_markers("section", &section_markers, &expected_sections)?;
    require_exact_markers("section-end", &section_end_markers, &expected_sections)?;
    for section in SECTION_NAMES {
        render(Some(section))?;
    }

    let command_markers = marker_counts("orch-guide-command:");
    let expected_commands = public_leaf_commands
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if expected_commands.len() != public_leaf_commands.len() {
        bail!("compiled CLI command tree contains duplicate public leaf paths");
    }
    require_exact_markers("command", &command_markers, &expected_commands)?;

    let invariant_markers = marker_counts("orch-guide-invariant:");
    let expected_invariants = REQUIRED_INVARIANTS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();
    require_exact_markers("invariant", &invariant_markers, &expected_invariants)?;

    // `wake status|attach|cancel|declare-dead` are intentionally parsed as a
    // positional virtual action, not Clap subcommands. Keep their product
    // contract in the same exact-coverage check as the real command tree.
    let wake_action_markers = marker_counts("orch-guide-wake-action:");
    let expected_wake_actions = wake_actions
        .iter()
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();
    if expected_wake_actions.len() != wake_actions.len() {
        bail!("compiled wake control action table contains duplicate names");
    }
    require_exact_markers("wake-action", &wake_action_markers, &expected_wake_actions)?;

    // Public exit semantics are a compiled table, not free-form prose. Keep the
    // shipped guide in exact two-way coverage with `CliDisposition::ALL`.
    let disposition_markers = marker_counts("orch-guide-disposition:");
    let disposition_names = orch_host::failure::CliDisposition::ALL
        .iter()
        .map(|disposition| disposition.name().to_string())
        .collect::<Vec<_>>();
    let expected_dispositions = disposition_names.iter().cloned().collect::<BTreeSet<_>>();
    if expected_dispositions.len() != disposition_names.len() {
        bail!("compiled CLI disposition table contains duplicate names");
    }
    require_exact_markers("disposition", &disposition_markers, &expected_dispositions)?;

    for forbidden in [
        "/Users/",
        "mainFull:",
        "round: r",
        "当前执行者容量",
        "当前轮次是",
    ] {
        if GUIDE_MARKDOWN.contains(forbidden) {
            bail!("portable guide contains dynamic or machine-local text {forbidden:?}");
        }
    }

    Ok(GuideStats {
        commands: expected_commands.len(),
        sections: expected_sections.len(),
        invariants: expected_invariants.len(),
        wake_actions: expected_wake_actions.len(),
        dispositions: expected_dispositions.len(),
    })
}

fn marker_counts(prefix: &str) -> BTreeMap<String, usize> {
    let line_prefix = format!("<!-- {prefix}");
    let mut counts = BTreeMap::new();
    for line in active_markdown().lines() {
        let line = line.trim();
        let Some(value) = line
            .strip_prefix(&line_prefix)
            .and_then(|value| value.strip_suffix(" -->"))
        else {
            continue;
        };
        *counts.entry(value.to_string()).or_insert(0) += 1;
    }
    counts
}

// Feature membership is explicit document data. Never filter markers against
// the live Clap tree: that would silently hide stale declarations and weaken
// the existing two-way coverage check.
fn active_markdown() -> String {
    let mut output = String::new();
    for line in GUIDE_MARKDOWN.lines() {
        if line.trim().starts_with("<!-- orch-guide-selfhost-command:") {
            if cfg!(feature = "selfhost") {
                output
                    .push_str(&line.replace("orch-guide-selfhost-command:", "orch-guide-command:"));
                output.push('\n');
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn require_exact_markers(
    label: &str,
    actual: &BTreeMap<String, usize>,
    expected: &BTreeSet<String>,
) -> Result<()> {
    let duplicates = actual
        .iter()
        .filter(|(_, count)| **count != 1)
        .map(|(name, count)| format!("{name}×{count}"))
        .collect::<Vec<_>>();
    if !duplicates.is_empty() {
        bail!(
            "guide {label} markers duplicated: {}",
            duplicates.join(", ")
        );
    }
    let actual = actual.keys().cloned().collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    let unknown = actual.difference(expected).cloned().collect::<Vec<_>>();
    if !missing.is_empty() || !unknown.is_empty() {
        bail!(
            "guide {label} coverage mismatch; missing=[{}] unknown=[{}]",
            missing.join(", "),
            unknown.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_section_is_extractable_and_portable() {
        for section in SECTION_NAMES {
            let rendered = render(Some(section)).unwrap();
            assert!(rendered.starts_with("## "), "section={section}: {rendered}");
            assert!(!rendered.contains("orch-guide-section:"));
        }
        assert!(render(Some("missing"))
            .unwrap_err()
            .to_string()
            .contains("未知"));
        assert!(!GUIDE_MARKDOWN.contains("/Users/"));
    }

    #[test]
    fn invariant_markers_are_unique() {
        let counts = marker_counts("orch-guide-invariant:");
        assert_eq!(counts.len(), REQUIRED_INVARIANTS.len());
        assert!(counts.values().all(|count| *count == 1));
    }

    #[test]
    fn positional_wake_actions_are_exactly_covered() {
        let counts = marker_counts("orch-guide-wake-action:");
        let actual = counts.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let expected = ["attach", "cancel", "declare-dead", "status"]
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert!(counts.values().all(|count| *count == 1));
    }

    #[test]
    fn cli_dispositions_are_exactly_covered() {
        let counts = marker_counts("orch-guide-disposition:");
        let actual = counts.keys().map(String::as_str).collect::<BTreeSet<_>>();
        let expected = orch_host::failure::CliDisposition::ALL
            .iter()
            .map(|disposition| disposition.name())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert!(counts.values().all(|count| *count == 1));
    }
}
