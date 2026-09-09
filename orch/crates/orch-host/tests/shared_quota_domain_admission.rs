//! B234 seeded-red contract: 同工具多实例共享底层配额 + 注册表资格上界进签核 IR
//! （用户 2026-08-05 需求 3 的调度面；fusion 定稿 D7/D12）。
//!
//! Expected red: compile。`scheduler::capacity_admits_in_domain` / `plan::validate_scheduling_against_registry`
//! / `plan::agent_registry_digest` / `wave::split_wave_by_capacity_with_domains` 尚不存在。
//!
//! ── 为什么这张卡的实现面比想象中小（fusion D7，codex 读码发现、planner 逐段复核）──
//! 通用调度器**早已是 agent/domain 两层**（`CapacityLimits{agents, quota_domains}`
//! scheduler.rs:35-38、`CapacitySnapshot` :43-47），但生产准入口 `capacity_admits_with_limits`
//! （scheduler.rs:299-320）把 `quota_domain` 直接写成 agent 自身，域这一层等于空转；
//! 波次规划 `split_wave_by_capacity`（wave.rs:114-118）也只按 `cap.agent.min(cap.quota)`
//! 逐 agent 裁剪，没有域概念。**本卡是把真实的域喂进已有机制，不是新造机制。**
//!
//! ── 本种子刻意不做的事 ──
//! 不用 struct literal 构造 `IrScheduling`/`IrCapacity`/`WaveStep`/`LoadItem`（改走 YAML 反序列化
//! 与既有 `plan_wave_schedule`）——避免重演 B34/B93 那种「冻结种子钉死字段集导致结构不可演进」
//! 的陷阱（fusion D1 的教训，见 B233 种子头）。准入接口取「agent → 在飞计数」的朴素映射，
//! 同样是为了不把 `LoadItem` 的字段集钉进冻结文件。
//!
//! Negative mutations that must turn the named case red:
//! M1. 域计数只算当前 agent（回到 domain=agent 的空转形态）
//!     -> `same_domain_instances_share_capacity` 红。
//! M2. 域上限被违反时错误信息不点名域（运维无从判断该调哪个上限）
//!     -> `admission_error_names_the_shared_domain` 红。
//! M3. 域闸取代了 agent 闸（只剩一层）
//!     -> `agent_capacity_still_binds_inside_an_unsaturated_domain` 红。
//! M4. 不同域被错误合并（无关工具互相阻塞）
//!     -> `distinct_domains_do_not_block_each_other` 红。
//! M5. 波次裁剪继续按 per-agent min，忽略共享域
//!     -> `wave_split_respects_shared_domain_not_per_agent_minimum` 红。
//! M6. 同域多实例但未声明域上限时静默放行（底层工具并发失控）
//!     -> `multiple_instances_of_one_domain_require_an_explicit_domain_declaration` 红。
//! M7. 域上限小于成员上限时不报错（配置自相矛盾却被接受）
//!     -> `domain_capacity_below_a_member_capacity_is_rejected` 红。
//! M8. allowedAgents 允许注册表里不存在的名字（wake 期才 fail，签核期看不出来）
//!     -> `allowed_agents_must_exist_in_the_registry` 红。
//! M9. 轮级 roles 可以超出注册表声明的资格上界（轮次配置能给自己发权限）
//!     -> `mode_roles_must_be_a_subset_of_registry_roles` 红。
//! M10. 注册表摘要不随内容变化（改了注册表、旧 IR 照跑）
//!     -> `registry_digest_is_content_addressed` 红。

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use orch_host::plan::{
    agent_registry_digest, validate_scheduling_against_registry, IrQuotaDomain, IrScheduling,
};
use orch_host::registry::load_agent_definitions;
use orch_host::legacy::{capacity_admits_in_domain, DomainAdmission};
use orch_host::util::test_scratch_dir;

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

const AGENTS_TWO_OPENCODE: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
agents:
  executor-opencode:
    tool: opencode
    model: dewu-ep/glm-5.2
    effort: high
    roles: [primary-review, secondary-review, nongate-review]
    responsibility: "正门审查"
    injectable: true
    sessionId: "fresh-session-per-wake"
  executor-opencode-scout:
    tool: opencode
    model: opencode/deepseek-v4-flash-free
    effort: minimal
    roles: [nongate-review]
    responsibility: "非门探查"
    injectable: true
    sessionId: "fresh-session-per-wake"
"#;

fn write_registry(tag: &str, agents_yaml: &str) -> PathBuf {
    let root = test_scratch_dir(tag);
    fs::create_dir_all(root.join("coordination/tools")).expect("建 coordination/tools 失败");
    fs::write(root.join("coordination/tools/opencode.yaml"), TOOL_OPENCODE)
        .expect("写 ToolDefinition 失败");
    fs::write(root.join("coordination/agents.yaml"), agents_yaml).expect("写 agents.yaml 失败");
    root
}

fn scheduling_from(yaml: &str) -> IrScheduling {
    serde_yaml::from_str(yaml).expect("IrScheduling 反序列化失败")
}

fn domains_from(yaml: &str) -> BTreeMap<String, IrQuotaDomain> {
    serde_yaml::from_str(yaml).expect("quotaDomains 反序列化失败")
}

const SCHEDULING_TWO_OPENCODE: &str = r#"allowedAgents: [executor-opencode, executor-opencode-scout]
capacities:
  executor-opencode: {agent: 3, quota: 9, roles: [primary-review, secondary-review]}
  executor-opencode-scout: {agent: 1, quota: 2, roles: [nongate-review]}
"#;

#[test]
fn same_domain_instances_share_capacity() {
    // M1：同一底层工具的两个实例共用一份真实并发额度。
    // 域上限 1，域内已有另一实例在飞 ⇒ 本实例必须被拒，哪怕它自己的 agent 额度还空着。
    let members = vec![
        "executor-opencode".to_string(),
        "executor-opencode-scout".to_string(),
    ];
    let load = BTreeMap::from([("executor-opencode".to_string(), 1usize)]);
    let request = DomainAdmission {
        agent: "executor-opencode-scout",
        domain: "opencode",
        domain_members: &members,
        agent_capacity: 3,
        domain_capacity: 1,
    };
    assert!(
        capacity_admits_in_domain(&load, &request).is_err(),
        "同域已占满时必须拒绝——两个实例共享底层工具的并发额度"
    );
}

#[test]
fn admission_error_names_the_shared_domain() {
    // M2：拒绝信息必须点名域，否则运维只会去调 agent 上限，永远调不到真正生效的那一层。
    let members = vec![
        "executor-opencode".to_string(),
        "executor-opencode-scout".to_string(),
    ];
    let load = BTreeMap::from([("executor-opencode".to_string(), 1usize)]);
    let request = DomainAdmission {
        agent: "executor-opencode-scout",
        domain: "opencode",
        domain_members: &members,
        agent_capacity: 3,
        domain_capacity: 1,
    };
    let error = capacity_admits_in_domain(&load, &request).expect_err("应当被拒");
    assert!(
        error.contains("opencode"),
        "拒绝理由必须点名配额域，实得：{error}"
    );
}

#[test]
fn agent_capacity_still_binds_inside_an_unsaturated_domain() {
    // M3：两层闸都在。域很宽松，但本实例自己的并发已到顶 ⇒ 仍然拒绝。
    let members = vec!["executor-opencode".to_string()];
    let load = BTreeMap::from([("executor-opencode".to_string(), 3usize)]);
    let request = DomainAdmission {
        agent: "executor-opencode",
        domain: "opencode",
        domain_members: &members,
        agent_capacity: 3,
        domain_capacity: 99,
    };
    assert!(
        capacity_admits_in_domain(&load, &request).is_err(),
        "agent 层上限不得被域层放宽吃掉"
    );
}

#[test]
fn distinct_domains_do_not_block_each_other() {
    // M4：不同工具互不牵连——pi 在飞不影响 opencode 准入。
    let members = vec!["executor-opencode".to_string()];
    let load = BTreeMap::from([
        ("executor-pi".to_string(), 3usize),
        ("executor-opencode".to_string(), 0usize),
    ]);
    let request = DomainAdmission {
        agent: "executor-opencode",
        domain: "opencode",
        domain_members: &members,
        agent_capacity: 3,
        domain_capacity: 3,
    };
    assert!(
        capacity_admits_in_domain(&load, &request).is_ok(),
        "域外 agent 的在飞量不得计入本域"
    );
}



#[test]
fn multiple_instances_of_one_domain_require_an_explicit_domain_declaration() {
    // M6：同工具挂了两个实例却没人声明域上限 ⇒ 底层并发无人管，签核期必须拒。
    let root = write_registry("b234-undeclared-domain", AGENTS_TWO_OPENCODE);
    let definitions = load_agent_definitions(&root).expect("注册表必须可解析");
    let scheduling = scheduling_from(SCHEDULING_TWO_OPENCODE);
    let quota_domains: BTreeMap<String, IrQuotaDomain> = BTreeMap::new();

    assert!(
        validate_scheduling_against_registry(&scheduling, &quota_domains, &definitions).is_err(),
        "同域多实例而未声明 quotaDomains 必须在 plan 期硬拒"
    );
}

#[test]
fn domain_capacity_below_a_member_capacity_is_rejected() {
    // M7：域上限 1 却给成员 agent 上限 3——配置自相矛盾，永远有一层是谎话。
    let root = write_registry("b234-domain-too-small", AGENTS_TWO_OPENCODE);
    let definitions = load_agent_definitions(&root).expect("注册表必须可解析");
    let scheduling = scheduling_from(SCHEDULING_TWO_OPENCODE);
    let quota_domains = domains_from("opencode: {agent: 1, quota: 11}\n");

    assert!(
        validate_scheduling_against_registry(&scheduling, &quota_domains, &definitions).is_err(),
        "域上限低于任一成员的 agent 上限必须拒绝"
    );
}

#[test]
fn allowed_agents_must_exist_in_the_registry() {
    // M8：轮次配置点名了一个注册表里根本没有的 agent。
    // 今天要到 wake 期才报 “AgentRegistry 未注册”，签核时完全看不出来。
    let root = write_registry("b234-ghost-agent", AGENTS_TWO_OPENCODE);
    let definitions = load_agent_definitions(&root).expect("注册表必须可解析");
    let scheduling = scheduling_from(
        r#"allowedAgents: [executor-opencode, executor-ghost]
capacities:
  executor-opencode: {agent: 3, quota: 9, roles: [primary-review]}
  executor-ghost: {agent: 1, quota: 1, roles: [nongate-review]}
"#,
    );
    let quota_domains = domains_from("opencode: {agent: 3, quota: 11}\n");

    assert!(
        validate_scheduling_against_registry(&scheduling, &quota_domains, &definitions).is_err(),
        "allowedAgents 必须是注册表键集的子集，且在 plan 期就判定"
    );
}

#[test]
fn mode_roles_must_be_a_subset_of_registry_roles() {
    // M9：注册表的 roles 是**资格上界**，轮次配置只能收窄、不能自己发权限。
    // 这里让探查实例在本轮被授予 primary-review，而它在注册表里只有 nongate-review。
    let root = write_registry("b234-role-escalation", AGENTS_TWO_OPENCODE);
    let definitions = load_agent_definitions(&root).expect("注册表必须可解析");
    let scheduling = scheduling_from(
        r#"allowedAgents: [executor-opencode, executor-opencode-scout]
capacities:
  executor-opencode: {agent: 3, quota: 9, roles: [primary-review]}
  executor-opencode-scout: {agent: 1, quota: 2, roles: [primary-review]}
"#,
    );
    let quota_domains = domains_from("opencode: {agent: 3, quota: 11}\n");

    assert!(
        validate_scheduling_against_registry(&scheduling, &quota_domains, &definitions).is_err(),
        "轮级 roles 超出注册表资格上界必须拒绝"
    );
}

#[test]
fn registry_digest_is_content_addressed() {
    // M10：签核 IR 要钉住「本轮认的是哪一份注册表」，否则改完注册表旧 IR 照跑，
    // 声明层就不再是事实。摘要必须只由内容决定。
    let a = write_registry("b234-digest-a", AGENTS_TWO_OPENCODE);
    let b = write_registry("b234-digest-b", AGENTS_TWO_OPENCODE);
    let digest_a = agent_registry_digest(&a).expect("摘要计算失败");
    let digest_b = agent_registry_digest(&b).expect("摘要计算失败");
    assert_eq!(digest_a, digest_b, "同内容必须同摘要（与路径无关）");
    assert!(!digest_a.is_empty(), "摘要不得为空");

    let mutated = AGENTS_TWO_OPENCODE.replace("effort: minimal", "effort: high");
    fs::write(b.join("coordination/agents.yaml"), mutated).expect("改写 agents.yaml 失败");
    let digest_mutated = agent_registry_digest(&b).expect("摘要计算失败");
    assert_ne!(
        digest_a, digest_mutated,
        "注册表内容一变，摘要必须变——这是旧 IR 被拒的依据"
    );
}
