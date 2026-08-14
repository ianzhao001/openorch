//! 高风险动作分类与审批事件载荷（design/08 §2）。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighRiskAction {
    Push,
    Publish,
    DeleteRecursive,
    Network,
    Install,
}

impl HighRiskAction {
    pub fn as_str(self) -> &'static str {
        match self {
            HighRiskAction::Push => "push",
            HighRiskAction::Publish => "publish",
            HighRiskAction::DeleteRecursive => "delete-recursive",
            HighRiskAction::Network => "network",
            HighRiskAction::Install => "install",
        }
    }
}

pub fn classify(cmd: &str) -> Option<HighRiskAction> {
    let tokens = cmd
        .split_whitespace()
        .map(normalize_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let command_index = executable_index(&tokens)?;
    let executable = program_name(&tokens[command_index]);
    let args = &tokens[command_index + 1..];

    if executable == "git" && args.iter().any(|arg| arg == "push") {
        return Some(HighRiskAction::Push);
    }

    if executable == "gh"
        && args
            .iter()
            .any(|arg| matches!(arg.as_str(), "pr" | "release"))
    {
        return Some(HighRiskAction::Publish);
    }
    if matches!(executable, "npm" | "cargo") && args.iter().any(|arg| arg == "publish") {
        return Some(HighRiskAction::Publish);
    }

    if executable == "rm" && args.iter().any(|arg| is_recursive_flag(arg)) {
        return Some(HighRiskAction::DeleteRecursive);
    }

    if matches!(executable, "curl" | "wget") {
        return Some(HighRiskAction::Network);
    }

    let installs = match executable {
        "npm" | "pnpm" | "cargo" | "pip" | "pip3" | "brew" | "apt" | "apt-get" | "gem" => {
            args.iter().any(|arg| arg == "install")
        }
        "yarn" => args
            .iter()
            .any(|arg| matches!(arg.as_str(), "add" | "install")),
        "python" | "python3" => {
            args.windows(2)
                .any(|pair| pair[0] == "-m" && matches!(pair[1].as_str(), "pip" | "pip3"))
                && args.iter().any(|arg| arg == "install")
        }
        _ => false,
    };
    installs.then_some(HighRiskAction::Install)
}

/// 高危分类去重（B72）：每条 cmd 过 `classify`，保留 Some，
/// 按变体去重保首现顺序。additive 纯函数，不改既有行为。
pub fn high_risk_kinds(cmds: &[String]) -> Vec<HighRiskAction> {
    let mut seen = Vec::new();
    for cmd in cmds {
        if let Some(action) = classify(cmd) {
            if !seen.contains(&action) {
                seen.push(action);
            }
        }
    }
    seen
}

pub fn request_payload(action: HighRiskAction, context: &str) -> serde_json::Value {
    serde_json::json!({
        "action": action.as_str(),
        "risk": "high",
        "context": context,
    })
}

pub fn is_decision_approved(payload: &serde_json::Value) -> bool {
    payload.get("decision").and_then(|value| value.as_str()) == Some("approved")
}

fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|character: char| {
            matches!(character, '\'' | '"' | '`' | ';' | '&' | '|' | '(' | ')')
        })
        .to_ascii_lowercase()
}

fn program_name(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn executable_index(tokens: &[String]) -> Option<usize> {
    let mut index = 0;
    while tokens.get(index).is_some_and(|token| token.contains('=')) {
        index += 1;
    }
    if tokens
        .get(index)
        .is_some_and(|token| program_name(token) == "env")
    {
        index += 1;
        while tokens.get(index).is_some_and(|token| token.contains('=')) {
            index += 1;
        }
    }
    if tokens
        .get(index)
        .is_some_and(|token| program_name(token) == "sudo")
    {
        index += 1;
        while tokens
            .get(index)
            .is_some_and(|token| token.starts_with('-'))
        {
            index += 1;
        }
    }
    (index < tokens.len()).then_some(index)
}

fn is_recursive_flag(arg: &str) -> bool {
    arg == "--recursive"
        || (arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.chars().skip(1).any(|flag| flag == 'r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_install_variants_without_flagging_test_commands() {
        assert_eq!(
            classify("cargo install cargo-nextest"),
            Some(HighRiskAction::Install)
        );
        assert_eq!(
            classify("python3 -m pip install httpx"),
            Some(HighRiskAction::Install)
        );
        assert_eq!(classify("cargo test --workspace"), None);
    }

    #[test]
    fn recognizes_recursive_delete_variants() {
        assert_eq!(
            classify("sudo rm -fr ./build"),
            Some(HighRiskAction::DeleteRecursive)
        );
        assert_eq!(
            classify("rm --recursive ./build"),
            Some(HighRiskAction::DeleteRecursive)
        );
        assert_eq!(classify("rm ./build.log"), None);
    }

    #[test]
    fn recognizes_publish_after_gh_global_options() {
        assert_eq!(
            classify("gh --repo owner/project release create v1"),
            Some(HighRiskAction::Publish)
        );
    }

    #[test]
    fn approval_decision_is_case_sensitive_and_type_strict() {
        assert!(is_decision_approved(
            &serde_json::json!({"decision": "approved"})
        ));
        assert!(!is_decision_approved(
            &serde_json::json!({"decision": "Approved"})
        ));
        assert!(!is_decision_approved(
            &serde_json::json!({"decision": true})
        ));
    }
}
