//! INBOX 指令 frontmatter 解析(design/11 §4,r21/B38)
//!
//! 指令文件 `coordination/inbox/<ts>-<slug>.md`:可选 frontmatter(priority/round-hint)
//! + 正文=自然语言指令。daemon 用 priority 给待处理队列排序,round-hint 供规划参考。
//! 本模块是纯函数解析:仅首个 `---`…`---` 块视为 frontmatter,正文内的 `---` 原样保留。

/// 解析后的 INBOX 指令。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Instruction {
    /// 队列排序优先级(frontmatter `priority`,缺省/非法 → None)
    pub priority: Option<u8>,
    /// 规划参考轮次提示(frontmatter `round-hint`,缺省 → None)
    pub round_hint: Option<String>,
    /// 正文(去首尾空白;内部 `---` 原样保留)
    pub body: String,
}

/// 解析 INBOX 指令文本:可选 frontmatter + 正文。
///
/// - src 以 `---\n` 开头 → 首个 `---`…`---` 块为 frontmatter(手解析 `priority`/`round-hint`),其后为正文;
/// - 否则整体即正文,priority/round_hint 皆 None;
/// - 正文去首尾空白,内部 `---` 不截断;frontmatter 缺键 → 该字段 None;priority 非 u8 → None。
pub fn parse_instruction(src: &str) -> Instruction {
    let Some(rest) = src.strip_prefix("---\n") else {
        return Instruction {
            body: src.trim().to_string(),
            ..Instruction::default()
        };
    };
    let Some(end) = rest.find("\n---") else {
        // 起始 --- 后无闭合块:整体视为正文,不做 frontmatter 解析
        return Instruction {
            body: src.trim().to_string(),
            ..Instruction::default()
        };
    };
    let fm = &rest[..end];
    let body = rest[end + 4..].trim().to_string();

    let mut ins = Instruction {
        body,
        ..Instruction::default()
    };
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("priority:") {
            ins.priority = v.trim().parse::<u8>().ok();
        } else if let Some(v) = line.strip_prefix("round-hint:") {
            let v = v.trim();
            if !v.is_empty() {
                ins.round_hint = Some(v.to_string());
            }
        }
    }
    ins
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_priority_only_leaves_that_field_none() {
        // 缺 priority:仅该字段 None,round-hint 正常解析
        let ins = parse_instruction("---\nround-hint: r23\n---\n正文\n");
        assert_eq!(ins.priority, None);
        assert_eq!(ins.round_hint.as_deref(), Some("r23"));
        assert_eq!(ins.body, "正文");
    }

    #[test]
    fn missing_round_hint_is_none() {
        let ins = parse_instruction("---\npriority: 7\n---\n正文\n");
        assert_eq!(ins.priority, Some(7));
        assert_eq!(ins.round_hint, None);
    }

    #[test]
    fn priority_boundary_and_invalid() {
        // u8 边界 0/255 可解析;256 溢出 → None;非数字 → None
        assert_eq!(parse_instruction("---\npriority: 0\n---\nx").priority, Some(0));
        assert_eq!(
            parse_instruction("---\npriority: 255\n---\nx").priority,
            Some(255)
        );
        assert_eq!(parse_instruction("---\npriority: 256\n---\nx").priority, None);
        assert_eq!(parse_instruction("---\npriority: high\n---\nx").priority, None);
    }
}
