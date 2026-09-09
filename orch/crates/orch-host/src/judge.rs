//! Legacy judge text helpers retained only for historical tests and readers.
//!
//! Schema-3 `consult` has no judge mode, prompt, subprocess or synthesis file;
//! root reads independent member artifacts and makes its own decision.

use crate::consult::{MemberOutcome, MemberStatus};

const SECTION_HEADINGS: [&str; 5] = [
    "Consensus",
    "Contradictions",
    "Partial coverage",
    "Unique insights",
    "Blind spots",
];

/// Maximum answer bytes contributed by one fusion member to a judge prompt.
///
/// The full answer remains in `fusion/<index>-<member>.md`; this only bounds the
/// synthesis input so one transcript-shaped response cannot crowd out peers.
pub const JUDGE_PROMPT_MEMBER_BUDGET: usize = 64 * 1024;

/// The terminal state of the optional synthesis step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeStatus {
    Planner,
    Skipped,
    Complete,
    Degraded,
    Failed,
}

/// A deliberately tolerant view over the judge's prose. The original text is
/// always retained, including when one or more requested sections are absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeSections {
    pub consensus: Option<String>,
    pub contradictions: Option<String>,
    pub partial_coverage: Option<String>,
    pub unique_insights: Option<String>,
    pub blind_spots: Option<String>,
    pub complete: bool,
    pub raw: String,
}

/// Build the comparison-only prompt consumed by either the planner or a local
/// judge adapter. It explicitly reserves final decision-making for the user.
pub fn judge_prompt(question: &str, outcomes: &[MemberOutcome]) -> String {
    let mut prompt = String::from(
        "You are the comparison judge for a planning consultation. Compare the fusion responses; do not make the final decision. The planner and user retain final authority.\n\nYour response must contain exactly these five second-level sections, in this order, with no additional `## ` sections:\n\n## Consensus\n\n## Contradictions\n\n## Partial coverage\n\n## Unique insights\n\n## Blind spots\n\nPlanning question and archived material:\n\n",
    );
    prompt.push_str(question);
    if !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str("\nFusion responses:\n");
    for outcome in outcomes {
        prompt.push_str(&format!(
            "\n### Member {} — {} ({})\n",
            outcome.index,
            outcome.member,
            member_status_name(outcome.status)
        ));
        let body = outcome
            .answer
            .as_deref()
            .or(outcome.reason.as_deref())
            .unwrap_or("No answer was returned.");
        let (bounded_body, truncated) = truncate_utf8_bytes(body, JUDGE_PROMPT_MEMBER_BUDGET);
        let fence = markdown_fence(bounded_body);
        prompt.push_str(&fence);
        prompt.push_str("text\n");
        prompt.push_str(bounded_body);
        if !bounded_body.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str(&fence);
        prompt.push('\n');
        if truncated {
            prompt.push_str(&format!(
                "⚠ 已截断，完整见 fusion/{}-{}.md\n",
                outcome.index, outcome.member
            ));
        }
    }
    prompt
}

/// Parse the five requested headings without rejecting imperfect model prose.
/// Presence of all five headings marks the result complete; otherwise callers
/// get a degraded structure plus the byte-for-byte original string.
pub fn parse_judge_sections(raw: &str) -> JudgeSections {
    let mut seen = [false; 5];
    let mut bodies = std::array::from_fn::<String, 5, _>(|_| String::new());
    let mut current = None;

    for line in raw.split_inclusive('\n') {
        let heading = line.trim_end_matches(['\r', '\n']);
        if let Some(index) = SECTION_HEADINGS
            .iter()
            .position(|expected| heading == format!("## {expected}"))
        {
            seen[index] = true;
            current = Some(index);
            continue;
        }
        if heading.starts_with("## ") {
            current = None;
            continue;
        }
        if let Some(index) = current {
            bodies[index].push_str(line);
        }
    }

    let section = |index: usize| seen[index].then(|| bodies[index].trim().to_string());
    JudgeSections {
        consensus: section(0),
        contradictions: section(1),
        partial_coverage: section(2),
        unique_insights: section(3),
        blind_spots: section(4),
        complete: seen.into_iter().all(|present| present),
        raw: raw.to_string(),
    }
}

fn member_status_name(status: MemberStatus) -> &'static str {
    match status {
        MemberStatus::Ok => "ok",
        MemberStatus::Failed => "failed",
        MemberStatus::TimedOut => "timed out",
    }
}

fn markdown_fence(text: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn truncate_utf8_bytes(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consult::{MemberOutcome, MemberStatus};

    #[test]
    fn prompt_names_exactly_the_five_comparison_sections() {
        let mut member = MemberOutcome::new(0, "consult-a", MemberStatus::Ok);
        member.answer = Some("answer".to_string());
        let prompt = judge_prompt("question", &[member]);
        let headings: Vec<_> = prompt
            .lines()
            .filter(|line| line.starts_with("## "))
            .collect();
        assert_eq!(
            headings,
            SECTION_HEADINGS
                .iter()
                .map(|heading| format!("## {heading}"))
                .collect::<Vec<_>>()
        );
        assert!(prompt.contains("planner and user retain final authority"));
    }

    #[test]
    fn missing_section_degrades_without_losing_original_text() {
        let raw = "preface\n## Consensus\nshared\n## Blind spots\nunknown\n";
        let parsed = parse_judge_sections(raw);
        assert!(!parsed.complete);
        assert_eq!(parsed.raw, raw);
        assert_eq!(parsed.consensus.as_deref(), Some("shared"));
        assert!(parsed.contradictions.is_none());

        let complete = "## Consensus\nc\n## Contradictions\nd\n## Partial coverage\np\n## Unique insights\nu\n## Blind spots\nb\n";
        assert!(parse_judge_sections(complete).complete);
    }
}
