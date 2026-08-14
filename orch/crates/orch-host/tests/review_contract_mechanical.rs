//! B201 · review 身份/解析/纯文本消费合同。
//!
//! 首红必须是 E0432，并在 rustc identity 中点名 verify/wake 四个缺失符号。
//!
//! M1. 恢复 deny_unknown_fields；M2. 任一 ReviewDelivered 生产者绕过共享 codec；
//! M3. runtime 模板重新信任 planner 消息里的 stale round/head；
//! M4. agy 纯文本判据退化为“有字节/有 substring”；
//! M5. agy 仍被要求提供不存在的 structured sentinel tool-output。

use orch_host::verify::{check_review_artifact_contract, ReviewContractExpectation};
use orch_host::wake::{
    plain_text_review_contract_consumed, render_review_frontmatter_template,
};

const HEAD: &str = "1111111111111111111111111111111111111111";
const WRONG: &str = "2222222222222222222222222222222222222222";

fn expected(role: &str, reviewer: &str) -> ReviewContractExpectation {
    ReviewContractExpectation::exact(
        "B201",
        "r62",
        "B201-A0001",
        role,
        reviewer,
        HEAD,
    )
    .unwrap()
}

#[test]
fn unknown_fields_are_tolerated_but_missing_reviewer_is_not() {
    let bytes = format!(
        "---\n\
         taskId: B201\nround: r62\nattemptId: B201-A0001\n\
         role: primary\nreviewer: executor-opencode\nverdict: PASS\n\
         reviewedHead: {HEAD}\n\
         verifier: executor-opencode\nindependence: isolated\n\
         ---\n\nsubstantive review\n"
    );
    let checked = check_review_artifact_contract(
        bytes.as_bytes(),
        &expected("primary", "executor-opencode"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        checked.ignored_fields(),
        &["independence".to_string(), "verifier".to_string()]
    );

    let missing = bytes.replace("reviewer: executor-opencode\n", "");
    assert!(check_review_artifact_contract(
        missing.as_bytes(),
        &expected("primary", "executor-opencode")
    )
    .is_err());
}

#[test]
fn runtime_template_contains_only_the_true_identity() {
    let text =
        render_review_frontmatter_template(&expected("primary", "executor-opencode")).unwrap();
    for line in [
        "taskId: B201",
        "round: r62",
        "attemptId: B201-A0001",
        "role: primary",
        "reviewer: executor-opencode",
        "verdict: __VERDICT__",
        "reviewedHead: 1111111111111111111111111111111111111111",
    ] {
        assert!(text.contains(line), "missing {line:?}: {text}");
    }
    assert!(!text.contains("round: r58"));
    assert!(!text.contains(WRONG));
}

#[test]
fn agy_plain_text_requires_exact_path_complete_identity_and_window() {
    let expected = expected("secondary", "executor-antigravity");
    let path =
        "/repo/coordination/rounds/r62/reviews/B201-A0001-secondary-executor-antigravity.md";
    let fm = render_review_frontmatter_template(&expected)
        .unwrap()
        .replace("__VERDICT__", "PASS");
    let log = format!("REVIEW_OUTPUT_PATH={path}\n```yaml\n{fm}```\n");
    assert!(plain_text_review_contract_consumed(
        log.as_bytes(),
        0,
        log.len() as u64,
        path,
        &expected
    )
    .unwrap());

    let wrong = log.replace(HEAD, WRONG);
    assert!(!plain_text_review_contract_consumed(
        wrong.as_bytes(),
        0,
        wrong.len() as u64,
        path,
        &expected
    )
    .unwrap());

    let substrings = format!(
        "REVIEW_OUTPUT_PATH={path}\nverdict: PASS\nreviewedHead: {HEAD}\n"
    );
    assert!(!plain_text_review_contract_consumed(
        substrings.as_bytes(),
        0,
        substrings.len() as u64,
        path,
        &expected
    )
    .unwrap());

    let stale_prefix = format!("{log}new window contains no contract\n");
    assert!(!plain_text_review_contract_consumed(
        stale_prefix.as_bytes(),
        log.len() as u64,
        stale_prefix.len() as u64,
        path,
        &expected
    )
    .unwrap());

    let partial = format!("REVIEW_OUTPUT_PATH={path}\n```yaml\n{fm}");
    assert!(!plain_text_review_contract_consumed(
        partial.as_bytes(),
        0,
        partial.len() as u64,
        path,
        &expected
    )
    .unwrap());
}
