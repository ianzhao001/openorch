//! 敏感信息滤网（B15q）：日志落盘前打码 API key / Bearer token（design/08 雏形）。
//! 契约：orch/crates/orch-host/tests/redact_log.rs（红种子逐字节落位）。
//! 纪律：纯 std 实现（writeSet 不含 Cargo.toml，不得新增依赖）。

/// 打码单行日志：
/// - `sk-` 前缀 API key → `sk-***`（保留前缀标识，密文不外泄）；
/// - `Bearer <token>` → `Bearer ***`；
/// - 无敏感内容原样返回；幂等（打码结果再过滤网不变，`***` 不属于密文字符集）。
pub fn redact_line(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < b.len() {
        // sk- 前缀 API key：词首边界 + 至少一个密文字符（不含 '*'，保证幂等）
        if at_word_start(b, i) && b[i..].starts_with(b"sk-") {
            let body = i + 3;
            let mut j = body;
            while j < b.len() && is_key_char(b[j]) {
                j += 1;
            }
            if j > body {
                out.push_str("sk-***");
                i = j;
                continue;
            }
        }
        // Bearer <token>：token 为一段非空白（'*' 重打码结果不变，保证幂等）
        if at_word_start(b, i) && b[i..].starts_with(b"Bearer ") {
            let tok = i + 7;
            let mut j = tok;
            while j < b.len() && !b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > tok {
                out.push_str("Bearer ***");
                i = j;
                continue;
            }
        }
        // 非敏感片段：按 UTF-8 字符边界原样搬运
        let n = utf8_len(b[i]);
        out.push_str(&line[i..i + n]);
        i += n;
    }
    out
}

/// 词首边界：行首或前一字符非单词字符（防 task-42 之类误伤）
fn at_word_start(b: &[u8], i: usize) -> bool {
    i == 0 || !is_word_char(b[i - 1])
}

fn is_word_char(x: u8) -> bool {
    x.is_ascii_alphanumeric() || x == b'_'
}

/// API key 密文字符集（不含 '*'/' '，保证打码产物不再被匹配）
fn is_key_char(x: u8) -> bool {
    x.is_ascii_alphanumeric() || x == b'_' || x == b'-'
}

/// 打码 KV 赋值对：键名（不区分大小写）以 TOKEN/SECRET/PASSWORD/KEY 结尾的，
/// 值替换为 `***`；其余键值原样。同行多对（空格分隔）各自独立判定。幂等。
pub fn redact_kv_secrets(line: &str) -> String {
    const SENSITIVE: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "KEY"];
    line.split(' ')
        .map(|token| {
            if let Some(eq) = token.find('=') {
                let key = &token[..eq];
                let ku = key.to_ascii_uppercase();
                if SENSITIVE.iter().any(|s| ku.ends_with(s)) {
                    let mut r = String::with_capacity(eq + 3);
                    r.push_str(key);
                    r.push_str("=***");
                    return r;
                }
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 组合两网（B15q sk-/Bearer + B30 KV）为单入口。
/// 策略：先 sk-/Bearer 打码，再 KV；KV 仅保留第一张网产生的精确 `sk-***` 标记。幂等。
pub fn redact_full(line: &str) -> String {
    let after_sk = redact_line(line);
    redact_kv_preserving_tokens(&after_sk)
}

/// 若组合脱敏网会改变该行，则该行包含可识别的敏感内容。
pub fn has_secret(line: &str) -> bool {
    redact_full(line) != line
}

/// KV 打码，但保留 sk 网已经产生的精确 `sk-***` 标记。
///
/// 不能用 `contains("sk-")` / `contains("Bearer")` 放行：未命中第一张网的
/// `xsk-raw` / `xBearerSecret` 仍必须由 KV 网兜底。
fn redact_kv_preserving_tokens(line: &str) -> String {
    const SENSITIVE: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "KEY"];
    line.split(' ')
        .map(|token| {
            if let Some(eq) = token.find('=') {
                let key = &token[..eq];
                let value = &token[eq + 1..];
                let ku = key.to_ascii_uppercase();
                if SENSITIVE.iter().any(|s| ku.ends_with(s)) && value != "sk-***" {
                    let mut r = String::with_capacity(eq + 3);
                    r.push_str(key);
                    r.push_str("=***");
                    return r;
                }
            }
            token.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 尾部保留打码（r40/B81 canary）：保留末 `keep` 个字符，其余每字符换 '*'。
/// `keep >= 字符数` ⇒ 原样返回；按字符计（多字节安全）。
pub fn mask_tail(s: &str, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    if keep >= len {
        return s.to_string();
    }
    let mask_count = len - keep;
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| if i < mask_count { '*' } else { *c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::redact_line;

    #[test]
    fn multiple_secrets_same_line_all_redacted() {
        // 同行多个 secret 全部打码
        let out = redact_line("k1=sk-aaa111 then Bearer tok.abc-9 end");
        assert!(out.contains("sk-***"), "sk key 应打码：{out}");
        assert!(out.contains("Bearer ***"), "Bearer 应打码：{out}");
        assert!(!out.contains("aaa111"), "sk 密文残留：{out}");
        assert!(!out.contains("tok.abc-9"), "Bearer 密文残留：{out}");
    }

    #[test]
    fn benign_sk_words_not_redacted() {
        // 普通含 sk 单词（task- 编号 / sketch / disk）不得误伤；已打码产物原样
        let line = "task-42 disk sketch 已滤 sk-***";
        assert_eq!(redact_line(line), line);
    }

    #[test]
    fn kv_monkey_key_edge_case() {
        // 边界：MONKEY 以 KEY 结尾，按规则应打码（尴尬但一致）
        use super::redact_kv_secrets;
        assert_eq!(redact_kv_secrets("MONKEY=banana"), "MONKEY=***");
        // 对比：不含敏感后缀的键不打码
        assert_eq!(redact_kv_secrets("MONK=banana"), "MONK=banana");
    }

    #[test]
    fn kv_case_insensitive_suffix() {
        // 大小写不敏感：token/secret/password/key 混合大小写均应识别
        use super::redact_kv_secrets;
        assert_eq!(redact_kv_secrets("api_token=v1"), "api_token=***");
        assert_eq!(redact_kv_secrets("API_TOKEN=v1"), "API_TOKEN=***");
        assert_eq!(redact_kv_secrets("db_Password=pw"), "db_Password=***");
        assert_eq!(redact_kv_secrets("my_key=k"), "my_key=***");
    }

    #[test]
    fn full_order_independent_when_no_overlap() {
        // 两网独立作用域时顺序不影响结果（KV+sk-/Bearer 无值重叠）
        use super::{redact_full, redact_kv_secrets, redact_line};
        let input = "GH_TOKEN=t9 Bearer xyz PATH=/bin";
        let full = redact_full(input);
        let kv_first = redact_line(&redact_kv_secrets(input));
        let sk_first = redact_kv_secrets(&redact_line(input));
        assert_eq!(full, kv_first);
        assert_eq!(full, sk_first);
    }

    #[test]
    fn full_empty_line_unchanged() {
        // 空行边界：不应 panic 或改变
        use super::redact_full;
        assert_eq!(redact_full(""), "");
        assert_eq!(redact_full("   "), "   ");
    }

    #[test]
    fn full_preserves_real_sk_marker_but_not_substring_bypasses() {
        use super::redact_full;
        assert_eq!(redact_full("API_KEY=sk-live999"), "API_KEY=sk-***");
        assert_eq!(redact_full("API_KEY=xsk-raw"), "API_KEY=***");
        assert_eq!(redact_full("API_TOKEN=xBearerSecret"), "API_TOKEN=***");
    }
}
