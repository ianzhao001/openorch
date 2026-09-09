//! 执行者探针解析（r26/B48）：解析 wake 探针回答行 "MODEL=<id> DEPTH=<档> [CWD=...]"。
//! 背景：r26 修订3 全员 model/思考深度交叉取证（coordination/archive/probe-model-r26.md，五员三员自报失真）。
//! （planner 预置占位：lib.rs 声明先行入库，B48 在本文件内实现，勿动 lib.rs——frozenPaths。）

/// 探针声明：从探针回答行解析出的 model 与 depth。
pub struct ProbeDecl {
    pub model: String,
    pub depth: String,
}

/// 解析单行探针回答。
///
/// 契约：MODEL/DEPTH 两键缺一不可；值为空视同缺失返 None；
/// 多余键（CWD 等）容忍忽略；首尾空白容忍。
/// 重复键取**首值**（first-wins）——探针回答是声明性的，
/// 首次出现即为真实意图，后续重复视为噪声/重放，不予覆盖。
/// 空值同样占位（first-wins 含空值），故 `MODEL= MODEL=b` 中首值为空 → None。
pub fn parse_probe_line(line: &str) -> Option<ProbeDecl> {
    let line = line.trim();
    let mut model: Option<String> = None;
    let mut depth: Option<String> = None;

    for token in line.split_whitespace() {
        if let Some(eq_pos) = token.find('=') {
            let key = &token[..eq_pos];
            let value = token[eq_pos + 1..].to_string();
            match key {
                "MODEL" if model.is_none() => model = Some(value),
                "DEPTH" if depth.is_none() => depth = Some(value),
                _ => {}
            }
        }
    }

    // 空值视同缺失：filter 掉空字符串。
    let model = model.filter(|m| !m.is_empty())?;
    let depth = depth.filter(|d| !d.is_empty())?;

    Some(ProbeDecl { model, depth })
}

/// 归一化模型 ID：trim → 去 provider 前缀（最后一个 '/' 前全部丢弃）→ 转小写。
///
/// 契约（B51 种子）：同一模型在不同通道写法不一
/// （"z-ai/glm-5.2" / "dewu-ep/glm-5.2" / "GLM-5.2"），
/// 归一化后均得 "glm-5.2" 以支撑 SOP 自动化的精确比对。
pub fn normalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let after_slash = match trimmed.rfind('/') {
        Some(pos) => &trimmed[pos + 1..],
        None => trimmed,
    };
    after_slash.to_lowercase()
}

/// 汇总探针声明中的去重模型清单（B71 契约）：每个 decl.model 过
/// `normalize_model_id`（去 provider 前缀、转小写），去重并**保首现顺序**。
pub fn distinct_models(decls: &[ProbeDecl]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for decl in decls {
        let norm = normalize_model_id(&decl.model);
        if !seen.iter().any(|m| m == &norm) {
            seen.push(norm);
        }
    }
    seen
}

/// 比对两个模型 ID 是否指向同一模型：两侧归一化后精确相等；
/// 任一侧归一化后为空 → false（缺值不得混过，镜像 parse_probe_line 的空值语义）。
pub fn model_matches(declared: &str, expected: &str) -> bool {
    let norm_declared = normalize_model_id(declared);
    let norm_expected = normalize_model_id(expected);
    if norm_declared.is_empty() || norm_expected.is_empty() {
        return false;
    }
    norm_declared == norm_expected
}

#[cfg(test)]
mod tests {
    use super::*;

    // 自写单测 1：键序颠倒也能正确解析（DEPTH 在前 MODEL 在后）。
    // 依据：探针回答行的键序不应被假定固定，解析器按 token 独立提取，
    //       任何顺序的 MODEL=/DEPTH= 都应正确配对。
    #[test]
    fn reversed_key_order_parses() {
        let p = parse_probe_line("DEPTH=high MODEL=glm-5.2")
            .expect("键序颠倒不应影响解析");
        assert_eq!(p.model, "glm-5.2");
        assert_eq!(p.depth, "high");
    }

    // 自写单测 2：重复键取首值（first-wins）。
    // 依据：first-wins 策略——首次出现为真实意图，后续重复为噪声。
    //       `MODEL=a MODEL=b` 应保留 a 而非 b。
    #[test]
    fn duplicate_key_takes_first() {
        let p = parse_probe_line("MODEL=alpha MODEL=beta DEPTH=mid")
            .expect("双键齐全应解析成功");
        assert_eq!(p.model, "alpha", "重复键取首值（first-wins）");
        assert_eq!(p.depth, "mid");
    }

    // 自写单测 3：首值为空时占位，后续有效值不覆盖 → None。
    // 依据：first-wins 含空值——空值已锁定该键槽位，后续值不覆盖，
    //       最终 filter 将空值剔除 → 缺失 → None。
    #[test]
    fn first_empty_value_blocks_later_valid() {
        assert!(
            parse_probe_line("MODEL= MODEL=real DEPTH=high").is_none(),
            "首值为空即占位，后续不覆盖，最终 None"
        );
    }

    // 自写单测 4：多级前缀 "a/b/c" 行为钉死。
    // 依据：归一化取最后一个 '/' 之后的部分——多级 provider 前缀应被完整剥离，
    //       "a/b/c" 归一化后为 "c" 而非 "b/c"。
    #[test]
    fn multi_level_prefix_stripped_to_last_segment() {
        assert_eq!(normalize_model_id("a/b/c"), "c");
        assert_eq!(normalize_model_id("provider/sub/glm-5.2"), "glm-5.2");
    }

    // 自写单测 5：normalize 幂等性——对已归一化的值再归一化应不变。
    // 依据：normalize_model_id 的输出（无 '/'、纯小写、已 trim）再次经过
    //       normalize 应保持不变，确保 model_matches 的双侧归一化安全。
    #[test]
    fn normalize_is_idempotent() {
        let once = normalize_model_id("z-ai/GLM-5.2");
        let twice = normalize_model_id(&once);
        assert_eq!(once, twice);
        assert_eq!(twice, "glm-5.2");
    }
}
