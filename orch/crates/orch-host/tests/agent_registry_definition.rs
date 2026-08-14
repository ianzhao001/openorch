//! B233 seeded-red contract: agent 注册表 v2 —— 一个 agent = 命名 + 工具 + model + effort + 职责，
//! 同工具多实例可并发，能显式指定的一律由 orch 注入（用户 2026-08-05 需求；fusion 定稿 D1/D2/D4/D5/D11）。
//!
//! Expected red: compile。`orch_host::registry::*` 尚不存在（E0432 unresolved import，文件级编译红）。
//!
//! ── 为什么是新模块而不是给 WakeSpec / AgentProfile 加字段（fusion D1，pi 提出、planner 抽验证实）──
//! `WakeSpec` 的三字段被 B34 种子 `tests/wake_render.rs:9-13` 以穷举 struct literal 钉死；
//! `AgentProfile` 的五字段被 B93 种子 `tests/agent_profile_validation.rs:24-29` 同样钉死。
//! 两个文件都是冻结种子（铁律 10），**给这两个结构加字段 = 永久门红**。
//! 因此本卡新建 `registry.rs`：`AgentDefinition` **嵌入** `AgentProfile`（不拓宽它），
//! 渲染产出独立的 `RenderedInvocation`（不拓宽 `WakeSpec`）。
//! 本种子第 10 个用例用穷举解构把这条约束反向锁死：AgentProfile 一旦新增字段，该行编译失败。
//!
//! ── 本种子自身刻意不钉的东西 ──
//! 不对 `RenderedInvocation` 之外的任何既有结构做穷举构造，避免再造一个同型冻结陷阱。
//!
//! Negative mutations that must turn the named case red:
//! M1. legacy（无 tool 字段）条目改走新渲染路径而与既有渲染器产生任何差异
//!     -> `legacy_entry_renders_byte_identical_to_todays_renderer` 红。
//! M2. legacy 条目也被注入 env（污染既有六席的运行环境）
//!     -> `legacy_entry_renders_byte_identical_to_todays_renderer` 红。
//! M3. 同工具两实例共用同一 model/effort（多实例形同虚设）
//!     -> `two_instances_of_one_tool_render_distinct_model_and_effort` 红。
//! M4. 注入改写 argv[0]（provider 分类器 wake.rs:6535-6590 按 basename 分流，改了即失去托管路径）
//!     -> `two_instances_of_one_tool_render_distinct_model_and_effort` 红。
//! M5. env 类工具把模型写进 argv（pi 无该旗标位，写进去就是错通道）
//!     -> `env_source_injects_canonical_and_tool_specific_names` 红。
//! M6. external-config 类被注入 model/effort（zcode/claw 的模型在仓外配置面，注入即撒谎）
//!     -> `external_config_source_injects_nothing` 红。
//! M7. 占位未解析却静默放行（老二进制会把字面 `{model}` 传给 CLI，新二进制不许重演）
//!     -> `argv_source_without_declared_model_fails_closed` 红。
//! M8. 未知 modelSource 静默降级而非整表拒载
//!     -> `unknown_model_source_rejects_the_whole_registry` 红。
//! M9. (tool, model, effort) 三元组重复不报错（两个同配实例语义歧义）
//!     -> `duplicate_tool_model_effort_triple_is_rejected` 红。
//! M10. 启动闸缺失或间隔取零（opencode 同刻双冷启动实测 `database is locked` 硬失败）
//!     -> `startup_gate_spaces_same_tool_launches` 红。
//!
//! 实测依据：`research/r65-opencode-multi-instance-probe.md`（双实例并发成立 / 同刻冷启动撞锁 /
//! observed 权威源）与 `research/r65-codex-pi-model-probe.md`（工具旗标面与 modelSource 归类）。

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use orch_host::agent_profile::{AgentProfile, QualityClass};
use orch_host::registry::{load_agent_definitions, plan_startup_wait, render_invocation};
use orch_host::util::test_scratch_dir;
use orch_host::wake::{self, WakeSpec};

const TOOL_OPENCODE: &str = r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: opencode
modelSource: argv
effortSemantic: variant
startupSpacingMs: 1200
maxConcurrent: 3
launch:
  argv: ["opencode", "run", "{message}", "--format", "json", "--dir", "{root}", "--model", "{model}", "--variant", "{effort}"]
observation:
  source: opencode-db
  policy: strict
"#;

const TOOL_PI: &str = r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: pi
modelSource: env
effortSemantic: thinking
startupSpacingMs: 0
maxConcurrent: 3
env:
  model: ORCH_PI_MODEL
  effort: ORCH_PI_EFFORT
launch:
  argv: ["sh", "coordination/scripts/wake-pi.sh", "{message}"]
observation:
  source: pi-frames
  policy: strict
"#;

const TOOL_ZCODE: &str = r#"apiVersion: orch/v1alpha1
kind: ToolDefinition
tool: zcode
modelSource: external-config
startupSpacingMs: 0
maxConcurrent: 2
launch:
  argv: ["sh", "orch/scripts/wake-zcode-stream.sh", "{message}"]
observation:
  source: none
  policy: advisory
"#;

/// legacy 条目：无 tool 字段、自带 wake.argv —— 既有六席在迁移前后都必须是这个行为。
const LEGACY_ARGV: [&str; 5] = ["agy", "-c", "-p", "{message}", "--dangerously-skip-permissions"];

const AGENTS_MIXED: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-legacy:
    injectable: true
    sessionId: "cwd-scoped--continue"
    wake:
      argv: ["agy", "-c", "-p", "{message}", "--dangerously-skip-permissions"]
  executor-opencode:
    tool: opencode
    model: dewu-ep/glm-5.2
    effort: high
    roles: [primary-review, secondary-review]
    responsibility: "正门审查：高可信复核"
    injectable: true
    sessionId: "fresh-session-per-wake"
  executor-opencode-scout:
    tool: opencode
    model: opencode/deepseek-v4-flash-free
    effort: minimal
    roles: [nongate-review]
    responsibility: "非门探查：低成本仓内侦察"
    injectable: true
    sessionId: "fresh-session-per-wake"
  executor-pi:
    tool: pi
    model: deepseek/deepseek-v4-flash
    effort: max
    roles: [nongate-review]
    responsibility: "非门审查：量化复测"
    injectable: true
    sessionId: "fresh-session-per-wake"
  executor-zcode:
    tool: zcode
    expectedModel: dcc-dewu-ep/gpt-5-dcc-glm-5-2
    expectedEffort: xhigh
    roles: [nongate-review, secondary-review]
    responsibility: "正门副审：深档复核"
    injectable: true
    sessionId: "fresh-session-per-wake"
"#;

fn write_fixture(tag: &str, tools: &[(&str, &str)], agents_yaml: &str) -> PathBuf {
    let root = test_scratch_dir(tag);
    fs::create_dir_all(root.join("coordination/tools")).expect("建 coordination/tools 失败");
    for (name, body) in tools {
        fs::write(root.join(format!("coordination/tools/{name}.yaml")), body)
            .expect("写 ToolDefinition 失败");
    }
    fs::write(root.join("coordination/agents.yaml"), agents_yaml).expect("写 agents.yaml 失败");
    root
}

fn all_tools() -> Vec<(&'static str, &'static str)> {
    vec![
        ("opencode", TOOL_OPENCODE),
        ("pi", TOOL_PI),
        ("zcode", TOOL_ZCODE),
    ]
}

#[test]
fn legacy_entry_renders_byte_identical_to_todays_renderer() {
    // M1/M2：迁移零破坏的机械锚——未声明 tool 的条目，渲染结果与今日 render_wake_argv
    // 逐字节相同，且不注入任何 env。既有六席在本卡合入当天必须一字不变地继续工作。
    let root = write_fixture("b233-legacy", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("含 legacy 条目的注册表必须可解析");
    let def = defs.get("executor-legacy").expect("缺 executor-legacy 条目");
    assert!(def.tool.is_none(), "无 tool 字段的条目必须保持 legacy 语义");

    let rendered = render_invocation(def, &root, "sess-legacy", "hello").expect("legacy 渲染失败");
    let spec = WakeSpec {
        injectable: true,
        session_id: "sess-legacy".to_string(),
        argv: LEGACY_ARGV.iter().map(|value| value.to_string()).collect(),
    };
    let baseline = wake::render_wake_argv(&spec, "hello").expect("既有渲染器失败");
    assert_eq!(
        rendered.argv, baseline,
        "legacy 条目的渲染必须与既有渲染器逐字节相同"
    );
    assert!(rendered.env.is_empty(), "legacy 条目不得被注入任何 env");
    assert!(rendered.requested_model.is_none(), "legacy 条目不声明模型");
}

#[test]
fn tool_template_entry_without_wake_key_loads_and_renders() {
    // 新式实例条目**不带 `wake` 键**，argv 取自 ToolDefinition.launch。
    // 这同时是老二进制的结构性防线（fusion D4）：老版 WakeTemplate.argv 是必填字段，
    // 读到这种条目会整表反序列化失败 ⇒ fail-loud 而非静默用错模型。
    let root = write_fixture("b233-tool-template", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("注册表必须可解析");
    let def = defs.get("executor-opencode").expect("缺 executor-opencode 条目");
    let rendered = render_invocation(def, &root, "sess-oc", "hello").expect("模板渲染失败");

    assert_eq!(rendered.argv[0], "opencode", "argv[0] 取自 ToolDefinition.launch");
    assert_eq!(rendered.argv[1], "run");
    assert!(
        rendered.argv.contains(&"hello".to_string()),
        "{{message}} 必须被替换"
    );
    assert!(
        rendered
            .argv
            .contains(&root.display().to_string()),
        "{{root}} 必须替换为仓根绝对路径（开源可移植性：模板内不留本机绝对路径）"
    );
    assert!(
        !rendered.argv.iter().any(|arg| arg.contains('{')),
        "渲染后不得残留任何占位：{:?}",
        rendered.argv
    );
}

#[test]
fn two_instances_of_one_tool_render_distinct_model_and_effort() {
    // M3/M4：需求的核心——同一工具、同一时刻、两套不同配置。
    // 实测背景：oc+glm 做审查与 oc+deepseek-free 做探查可真实并发
    // （research/r65-opencode-multi-instance-probe.md，overlap 实证）。
    let root = write_fixture("b233-two-instances", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("注册表必须可解析");
    let primary = defs.get("executor-opencode").expect("缺主实例");
    let scout = defs.get("executor-opencode-scout").expect("缺探查实例");

    let a = render_invocation(primary, &root, "sess-a", "m").expect("主实例渲染失败");
    let b = render_invocation(scout, &root, "sess-b", "m").expect("探查实例渲染失败");

    assert_eq!(a.requested_model.as_deref(), Some("dewu-ep/glm-5.2"));
    assert_eq!(
        b.requested_model.as_deref(),
        Some("opencode/deepseek-v4-flash-free")
    );
    assert_eq!(a.requested_effort.as_deref(), Some("high"));
    assert_eq!(b.requested_effort.as_deref(), Some("minimal"));
    assert!(a.argv.contains(&"dewu-ep/glm-5.2".to_string()));
    assert!(b.argv.contains(&"opencode/deepseek-v4-flash-free".to_string()));
    assert!(a.argv.contains(&"high".to_string()));
    assert!(b.argv.contains(&"minimal".to_string()));

    assert_eq!(
        a.argv[0], b.argv[0],
        "同工具两实例的 argv[0] 必须一致——provider 分类器按 basename 分流，改了就掉出托管路径"
    );
    assert_eq!(
        primary.profile.quota_domain, scout.profile.quota_domain,
        "同工具两实例必须落在同一配额域（底层工具并发是共享的）"
    );
    assert_eq!(
        a.env.get("ORCH_AGENT_NAME").map(String::as_str),
        Some("executor-opencode")
    );
    assert_eq!(
        b.env.get("ORCH_AGENT_NAME").map(String::as_str),
        Some("executor-opencode-scout")
    );
}

#[test]
fn env_source_injects_canonical_and_tool_specific_names() {
    // M5：pi 走包装脚本，模型只能穿 env（wake-pi.sh:51 已有 ORCH_PI_MODEL 读取位）。
    // 规范名让 orch 侧零方言知识，工具专用名由 ToolDefinition 声明映射。
    let root = write_fixture("b233-env-source", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("注册表必须可解析");
    let def = defs.get("executor-pi").expect("缺 executor-pi 条目");
    let rendered = render_invocation(def, &root, "sess-pi", "m").expect("env 类渲染失败");

    assert_eq!(
        rendered.env.get("ORCH_AGENT_MODEL").map(String::as_str),
        Some("deepseek/deepseek-v4-flash")
    );
    assert_eq!(
        rendered.env.get("ORCH_AGENT_EFFORT").map(String::as_str),
        Some("max")
    );
    assert_eq!(
        rendered.env.get("ORCH_PI_MODEL").map(String::as_str),
        Some("deepseek/deepseek-v4-flash"),
        "ToolDefinition 声明的工具专用 env 名必须同时注入"
    );
    assert_eq!(
        rendered.env.get("ORCH_PI_EFFORT").map(String::as_str),
        Some("max")
    );
    assert!(
        !rendered.argv.iter().any(|arg| arg.contains("deepseek")),
        "env 类工具不得把模型写进 argv：{:?}",
        rendered.argv
    );
    assert_eq!(rendered.requested_model.as_deref(), Some("deepseek/deepseek-v4-flash"));
}

#[test]
fn external_config_source_injects_nothing() {
    // M6：zcode 的模型/深度钉在 ~/.zcode/cli/config.json（无 CLI 旗标位）。
    // orch 只声明**期望值**供事后勾稽，绝不假称自己请求过——否则账本会记下一个从未发生的请求。
    let root = write_fixture("b233-external", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("注册表必须可解析");
    let def = defs.get("executor-zcode").expect("缺 executor-zcode 条目");
    let rendered = render_invocation(def, &root, "sess-z", "m").expect("external 类渲染失败");

    assert!(
        rendered.requested_model.is_none(),
        "external-config 类不得声称 orch 请求了模型"
    );
    assert!(rendered.requested_effort.is_none());
    assert!(!rendered.env.contains_key("ORCH_AGENT_MODEL"));
    assert!(!rendered.env.contains_key("ORCH_AGENT_EFFORT"));
    assert!(
        !rendered.argv.iter().any(|arg| arg.contains("gpt-5")),
        "external-config 类不得把期望模型注入 argv：{:?}",
        rendered.argv
    );
    assert_eq!(
        rendered.env.get("ORCH_AGENT_NAME").map(String::as_str),
        Some("executor-zcode"),
        "实例身份 env 对所有带 profile 的条目都要注入（多实例诊断用）"
    );
    assert_eq!(
        def.expected_model.as_deref(),
        Some("dcc-dewu-ep/gpt-5-dcc-glm-5-2"),
        "期望值必须保留，供 B235 的 advisory 勾稽使用"
    );
}

#[test]
fn argv_source_without_declared_model_fails_closed() {
    // M7：模板含 {model} 但条目未声明 model —— 老二进制会把字面 `{model}` 传给 CLI，
    // 新二进制必须在 load 或 render 任一处硬拒，绝不渲染出残留占位。
    let agents = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-opencode-nomodel:
    tool: opencode
    effort: high
    roles: [nongate-review]
    responsibility: "缺 model 的坏条目"
    injectable: true
    sessionId: "fresh-session-per-wake"
"#;
    let root = write_fixture("b233-nomodel", &all_tools(), agents);
    match load_agent_definitions(&root) {
        Err(_) => {}
        Ok(defs) => {
            let def = defs.get("executor-opencode-nomodel").expect("缺条目");
            let rendered = render_invocation(def, &root, "s", "m");
            assert!(
                rendered.is_err(),
                "未声明 model 却使用含 {{model}} 的模板必须 fail-closed，实得 {rendered:?}"
            );
        }
    }
}

#[test]
fn unknown_model_source_rejects_the_whole_registry() {
    // M8：未知枚举值一律整表拒载（fail-closed），不得降级为「按 legacy 处理」。
    let bad_tool = TOOL_OPENCODE.replace("modelSource: argv", "modelSource: telepathy");
    let root = write_fixture("b233-badsource", &[("opencode", bad_tool.as_str())], AGENTS_MIXED);
    assert!(
        load_agent_definitions(&root).is_err(),
        "未知 modelSource 必须让整张注册表解析失败"
    );
}

#[test]
fn external_config_tool_template_must_not_carry_injection_placeholders() {
    // external-config 与注入占位在语义上互斥：声明了「模型在仓外」却又留注入位，
    // 是配置自相矛盾，必须在 load 期拒绝而不是运行时才发现。
    let bad_tool = TOOL_ZCODE.replace(
        r#"argv: ["sh", "orch/scripts/wake-zcode-stream.sh", "{message}"]"#,
        r#"argv: ["sh", "orch/scripts/wake-zcode-stream.sh", "{message}", "--model", "{model}"]"#,
    );
    let root = write_fixture("b233-external-placeholder", &[("zcode", bad_tool.as_str())], AGENTS_MIXED);
    assert!(
        load_agent_definitions(&root).is_err(),
        "external-config 工具模板含注入占位必须整表拒载"
    );
}

#[test]
fn duplicate_tool_model_effort_triple_is_rejected() {
    // M9：两个实例同工具同模型同档位 ⇒ 语义歧义（调度层无从区分意图）。
    // 需要同配复用时必须用职责后缀把名字本身消歧。
    let agents = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-opencode:
    tool: opencode
    model: dewu-ep/glm-5.2
    effort: high
    roles: [primary-review]
    responsibility: "甲"
    injectable: true
    sessionId: "fresh-session-per-wake"
  executor-opencode-clone:
    tool: opencode
    model: dewu-ep/glm-5.2
    effort: high
    roles: [nongate-review]
    responsibility: "乙"
    injectable: true
    sessionId: "fresh-session-per-wake"
"#;
    let root = write_fixture("b233-dup-triple", &all_tools(), agents);
    assert!(
        load_agent_definitions(&root).is_err(),
        "(tool, model, effort) 三元组必须全表唯一"
    );
}

#[test]
fn startup_gate_spaces_same_tool_launches() {
    // M10：opencode 两个实例**同刻冷启动**实测硬失败——
    // `Error: Unexpected error / database is locked`（57 字节全文，无重试），
    // 错峰 ≥1s 后连续三次成功（research/r65-opencode-multi-instance-probe.md）。
    // ⇒ 启动时序也归配额域管：同工具冷启动必须按 ToolDefinition.startupSpacingMs 错峰。
    let root = write_fixture("b233-startup-gate", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("注册表必须可解析");
    let scout = defs.get("executor-opencode-scout").expect("缺探查实例");
    let base = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

    let waited = plan_startup_wait(scout, Some(base), base + Duration::from_millis(200))
        .expect("启动闸决策失败");
    assert_eq!(
        waited,
        Duration::from_millis(1000),
        "距上次同工具启动 200ms、间隔 1200ms ⇒ 还需等 1000ms"
    );

    let elapsed = plan_startup_wait(scout, Some(base), base + Duration::from_millis(1200))
        .expect("启动闸决策失败");
    assert_eq!(elapsed, Duration::ZERO, "已满 spacing ⇒ 不再等待");

    let first: Duration =
        plan_startup_wait(scout, None, base).expect("无历史启动时不应报错");
    assert_eq!(first, Duration::ZERO, "本域首次启动不等待");

    let pi = defs.get("executor-pi").expect("缺 executor-pi 条目");
    let no_gate = plan_startup_wait(pi, Some(base), base + Duration::from_millis(1))
        .expect("spacing=0 的工具不应报错");
    assert_eq!(
        no_gate,
        Duration::ZERO,
        "未声明冷启动间隔的工具不得被无端串行化"
    );
}

#[test]
fn agent_definition_embeds_agent_profile_without_widening_it() {
    // fusion D1 的反向锁：AgentProfile 一旦新增字段，下面这行穷举解构立即编译失败，
    // 从而在本卡的门上就暴露「有人又想去拓宽被 B93 种子钉死的结构」。
    let root = write_fixture("b233-profile-embed", &all_tools(), AGENTS_MIXED);
    let defs = load_agent_definitions(&root).expect("注册表必须可解析");
    let def = defs.get("executor-opencode-scout").expect("缺探查实例");

    let AgentProfile {
        id,
        capabilities,
        quota_domain,
        max_concurrent,
        quality_class,
    } = def.profile.clone();

    assert_eq!(id, "executor-opencode-scout", "profile.id = 注册表键");
    assert_eq!(
        capabilities,
        vec!["nongate-review".to_string()],
        "capabilities 承接注册表声明的 roles"
    );
    assert_eq!(quota_domain, "opencode", "quotaDomain = 工具名（同工具共享域）");
    assert_eq!(max_concurrent, 3, "max_concurrent 取 ToolDefinition.maxConcurrent");
    assert_eq!(quality_class, QualityClass::Standard, "v1 一律 Standard，质量分级另排");
    assert_eq!(
        def.responsibility, "非门探查：低成本仓内侦察",
        "职责定义必须被保留（人读字段，任何机械闸都不消费它）"
    );
}
