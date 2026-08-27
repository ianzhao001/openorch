//! B285 · 受审计的 model/effort 轮中修订入口 —— 冻结契约种子。
//!
//! **首红：compile `error[E0583]: file for module `audited_agent_pin_amendment_support` not found`。**
//! support 只是零判别力管道 marker（`pub fn contract_loaded() {}`），落点是目录形态
//! `tests/audited_agent_pin_amendment_support/mod.rs`，不新增 Cargo target。
//!
//! # 本卡的机械强度来自哪里（必须读懂再审）
//!
//! 用户要的是「轮中换 model/effort 不需要 replan」。有两条**看起来能达成、但必须排除**的做法：
//!
//! 1. **把三字段移出 registry digest 覆盖面** —— 那等于撤销 B283（r74 merge `f245d57e`）
//!    刚建立的 signed provider/model/effort 绑定；
//! 2. **修订入口自动补一条 `PlanSignedOff`** —— 那是伪造用户意志
//!    （签核是 HITL#1、`actor=user`、「永不被其他命令隐式触发」）。
//!
//! 本种子钉的是**第三条路**：签名 IR 绑定**起始** registry 摘要，每次修订落一条 durable
//! 事件作为**增量**，只读校验按账本顺序**折叠**出期望摘要再比对。
//! 这一模型在本仓已有先例——落地种子的有效 anchor 就是「genesis + 其后 durable
//! `SeedRelocated` 增量」（`oracle.rs::expected_recorded_dynamic_anchors`）。
//!
//! ⇒ **判据是合取**：`折叠后摘要与工作树一致`（本种子守）
//!    ∧ `没有事件就仍然失配`（本种子的负例守）
//!    ∧ `只有三字段能变`（本种子守）。缺任何一条，「不需要 replan」都会变成「校验被关掉」。
//!
//! # ⚠️ 诚实边界（写死在这里）
//!
//! - **不判上游可用性**。zcode 本轮的 `max` 是前瞻性钉死，网关兼容由用户另行修复；
//!   本卡只保证「改得动、改得可审计」，不保证「改完模型真能跑」。
//! - **不同步 `wake.argv` 里的模型字面量**。`executor-desktop` 的 argv 内联着 `-m`，
//!   这类席位必须被**显式拒绝**并要求走完整 replan，绝不允许签名面与实际调用不一致。
//! - **接线断言故意读源码，不自调**。折叠函数自己调自己永远能证明它工作，
//!   但证明不了**只读校验真的用了它**；也证明不了回执对账没有改用当前 registry。
//!   见 `the_readonly_validation_folds_amendments` 与
//!   `backend_reconciliation_still_reads_the_recorded_wake_pin`。
//!
//! # M 变异 ↔ 载体 一一对应
//!
//! | M | 注入 | 必红的载体 |
//! |---|---|---|
//! | M1 | 修订时不落事件，只改文件 | `a_hand_edit_without_an_amendment_event_still_drifts` |
//! | M2 | 折叠忽略账本顺序（取最后一条而非按序折叠） | `amendments_fold_in_ledger_order` |
//! | M3 | 把三字段移出 registry digest 覆盖面 | `the_registry_digest_covers_the_whole_registry_bytes` |
//! | M4 | 修订入口自动补 `PlanSignedOff` | `an_amendment_never_signs_the_plan_itself` |
//! | M5 | 用 parse→serialize 往返写盘 | `the_registry_comments_survive_an_amendment` |
//! | M6 | 放行第四个字段 | `only_the_three_pin_fields_may_be_amended` |
//! | M7 | 回执对账改用当前 registry | `backend_reconciliation_still_reads_the_recorded_wake_pin` |
//! | M8 | 非法状态也放行修订 | `the_amendment_entry_fails_closed_on_illegal_states` |
//! | M9 | 事务中途失败留下半改文件 | `a_refused_amendment_leaves_the_registry_byte_identical` |
//!
//! 每条断言写成一个独立 `#[test]`，失败点可单独定位（RUNBOOK 坑 16）。

#![allow(dead_code)]

mod audited_agent_pin_amendment_support;

use std::fs;
use std::path::{Path, PathBuf};

use orch_host::plan::{agent_registry_digest, expected_registry_digest_with_amendments};
use orch_host::registry::{amend_agent_pin, load_agent_definitions};

// ---------------------------------------------------------------- 公共工具

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("CARGO_MANIFEST_DIR 上溯三级应为仓根")
        .to_path_buf()
}

fn read_repo(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("读 {rel} 失败: {error}"))
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "orch-b285-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("coordination")).unwrap();
    root
}

/// 带**大量注释与空行**的 fixture registry。注释是本卡要保护的对象，不是装饰。
const FIXTURE_REGISTRY: &str = r#"apiVersion: orch/v1alpha1
kind: AgentRegistry
# 顶部注释：这一行必须活过修订。

agents:
  executor-alpha:
    # alpha 的事故史注释块 —— 机构记忆，抹掉即判 FAIL。
    #   第二行。
    injectable: true
    sessionId: "fresh-session-per-wake"
    provider: prov-old
    model: model-old
    effort: xhigh
    observation:
      source: alpha-frames
      policy: advisory
    wake: {argv: ["sh", "orch/scripts/wake-alpha.sh", "{message}"]}
    pokeHint: "alpha"

  executor-beta:
    # beta 的注释同样必须活着。
    injectable: true
    sessionId: "fresh-session-per-wake"
    wake: {argv: ["beta", "-p", "{message}", "-m", "beta-model-literal"]}
    pokeHint: "beta"
"#;

fn write_fixture_registry(root: &Path) {
    fs::write(root.join("coordination/agents.yaml"), FIXTURE_REGISTRY).unwrap();
}

fn registry_text(root: &Path) -> String {
    fs::read_to_string(root.join("coordination/agents.yaml")).unwrap()
}

// ------------------------------------------------- ① 覆盖面不得被削弱

/// M3 的载体：三字段必须**仍然**在 registry digest 的覆盖面内。
/// 把它们移出覆盖面是「让问题消失」而不是「解决问题」。
#[test]
fn the_registry_digest_covers_the_whole_registry_bytes() {
    let root = temp_root("digest-scope");
    write_fixture_registry(&root);
    let before = agent_registry_digest(&root).expect("fixture registry 应可摘要");

    // 只改 model 一个字段的字节。
    let mutated = registry_text(&root).replace("model: model-old", "model: model-new");
    fs::write(root.join("coordination/agents.yaml"), &mutated).unwrap();
    let after = agent_registry_digest(&root).expect("mutated registry 应可摘要");

    assert_ne!(
        before, after,
        "provider/model/effort 必须**仍然**进 registry digest 的覆盖面。\
         把它们摘出去确实能让「改了也不失配」，但那等于撤销 B283 建立的 signed 绑定——\
         本卡要的是可审计的增量，不是解绑"
    );

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ② 折叠模型

#[test]
fn an_amendment_keeps_the_folded_digest_in_sync() {
    let root = temp_root("fold-sync");
    write_fixture_registry(&root);
    let genesis = agent_registry_digest(&root).expect("genesis 摘要");

    let record = amend_agent_pin(
        &root,
        "executor-alpha",
        None,
        Some("model-new"),
        Some("max"),
        "r75 用户裁定：轮中换模型",
    )
    .expect("合法修订必须成功");

    let current = agent_registry_digest(&root).expect("修订后摘要");
    let folded = expected_registry_digest_with_amendments(&genesis, std::slice::from_ref(&record))
        .expect("折叠必须成功");

    assert_eq!(
        folded, current,
        "折叠出的期望摘要必须等于工作树的实际摘要——这正是「不需要 replan」的机械依据。\
         genesis={genesis} folded={folded} current={current}"
    );
    assert_ne!(
        genesis, current,
        "修订必须真的改变了 registry 字节；同值空转应在入口被拒，不该走到这里"
    );

    fs::remove_dir_all(root).unwrap();
}

/// M1 的载体：**没有事件就必须仍然失配**。
/// 这条负例是全卡最关键的一条——它证明「不需要 replan」是靠事件挣来的，不是靠关掉校验。
#[test]
fn a_hand_edit_without_an_amendment_event_still_drifts() {
    let root = temp_root("no-event");
    write_fixture_registry(&root);
    let genesis = agent_registry_digest(&root).expect("genesis 摘要");

    // 手工改同样的字节，但**不经修订入口**、不落事件。
    let mutated = registry_text(&root).replace("model: model-old", "model: model-new");
    fs::write(root.join("coordination/agents.yaml"), &mutated).unwrap();
    let current = agent_registry_digest(&root).expect("手改后摘要");

    let folded = expected_registry_digest_with_amendments(&genesis, &[]).expect("空链折叠");
    assert_eq!(
        folded, genesis,
        "空修订链的折叠结果必须等于 genesis 本身"
    );
    assert_ne!(
        folded, current,
        "没有 durable 修订事件的手工改动**必须仍然失配**。\
         若这里相等，说明校验被关掉了，而不是修订被审计了"
    );

    fs::remove_dir_all(root).unwrap();
}

/// M2 的载体：多次修订必须**按账本顺序**折叠。
#[test]
fn amendments_fold_in_ledger_order() {
    let root = temp_root("fold-order");
    write_fixture_registry(&root);
    let genesis = agent_registry_digest(&root).expect("genesis 摘要");

    let first = amend_agent_pin(&root, "executor-alpha", None, Some("model-mid"), None, "第一次")
        .expect("第一次修订");
    let second = amend_agent_pin(&root, "executor-alpha", None, Some("model-final"), None, "第二次")
        .expect("第二次修订");
    let current = agent_registry_digest(&root).expect("两次修订后摘要");

    let ordered = expected_registry_digest_with_amendments(
        &genesis,
        &[first.clone(), second.clone()],
    )
    .expect("按序折叠");
    assert_eq!(ordered, current, "按账本顺序折叠必须收敛到当前摘要");

    let reversed = expected_registry_digest_with_amendments(&genesis, &[second, first]);
    match reversed {
        Ok(value) => assert_ne!(
            value, current,
            "乱序折叠不得也收敛到当前摘要——否则顺序就没有被真正消费"
        ),
        Err(_) => {}
    }

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ③ 只允许三字段

#[test]
fn only_the_three_pin_fields_may_be_amended() {
    let root = temp_root("three-fields");
    write_fixture_registry(&root);

    let before = load_agent_definitions(&root).expect("修订前可加载");
    amend_agent_pin(&root, "executor-alpha", Some("prov-new"), Some("model-new"), Some("max"), "三字段")
        .expect("三字段修订必须成功");
    let after = load_agent_definitions(&root).expect("修订后可加载");

    assert_eq!(before.len(), after.len(), "修订不得增删 agent");
    for (id, before_def) in &before {
        let after_def = after.get(id).unwrap_or_else(|| panic!("agent {id} 消失了"));
        if id == "executor-alpha" {
            assert_eq!(after_def.model.as_deref(), Some("model-new"));
            assert_eq!(after_def.effort.as_deref(), Some("max"));
        } else {
            assert_eq!(
                format!("{before_def:?}"),
                format!("{after_def:?}"),
                "未被指名的 agent {id} 必须逐字段不变"
            );
        }
        assert_eq!(
            before_def.roles, after_def.roles,
            "roles 永远不得被本入口改动: {id}"
        );
        assert_eq!(
            before_def.profile.quota_domain, after_def.profile.quota_domain,
            "quotaDomain 永远不得被本入口改动: {id}"
        );
        assert_eq!(
            before_def.legacy_wake.is_some(),
            after_def.legacy_wake.is_some(),
            "wake 形态永远不得被本入口改动: {id}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

/// `executor-beta` 的 argv 内联了模型字面量。这类席位必须被**显式拒绝**，
/// 否则签名面会与实际调用不一致（`executor-desktop` 的 `-m` 就是现实例子）。
#[test]
fn an_inline_argv_model_literal_is_refused() {
    let root = temp_root("argv-literal");
    write_fixture_registry(&root);
    let before = registry_text(&root);

    let outcome = amend_agent_pin(&root, "executor-beta", None, Some("beta-model-new"), None, "应被拒");
    assert!(
        outcome.is_err(),
        "wake.argv 内联模型字面量的席位必须显式拒绝并要求走完整 replan；\
         默默只改签名面会让它与实际调用不一致"
    );
    assert_eq!(
        before,
        registry_text(&root),
        "被拒绝的修订不得留下任何字节改动"
    );

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ④ 注释必须活下来

#[test]
fn the_registry_comments_survive_an_amendment() {
    let root = temp_root("comments");
    write_fixture_registry(&root);
    let before = registry_text(&root);

    amend_agent_pin(&root, "executor-alpha", None, Some("model-new"), None, "改模型")
        .expect("合法修订");
    let after = registry_text(&root);

    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    assert_eq!(
        before_lines.len(),
        after_lines.len(),
        "外科式编辑不得增删行。行数变了说明走了 parse→serialize 往返，注释已经没了"
    );
    let differing: Vec<usize> = before_lines
        .iter()
        .zip(after_lines.iter())
        .enumerate()
        .filter(|(_, (b, a))| b != a)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        differing.len(),
        1,
        "只有目标字段那一行可以变，实际变了 {} 行: {differing:?}",
        differing.len()
    );

    for needle in [
        "# 顶部注释：这一行必须活过修订。",
        "# alpha 的事故史注释块 —— 机构记忆，抹掉即判 FAIL。",
        "#   第二行。",
        "# beta 的注释同样必须活着。",
    ] {
        assert!(
            after.contains(needle),
            "注释被抹掉了: {needle}。agents.yaml 是本项目的机构记忆，\
             每个席位都带大段事故史与裁定留证，抹掉即判 FAIL"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ⑤ 可审计与 fail-closed

#[test]
fn an_amendment_is_durable_and_attributable() {
    let root = temp_root("attributable");
    write_fixture_registry(&root);
    let genesis = agent_registry_digest(&root).expect("genesis");

    let record = amend_agent_pin(&root, "executor-alpha", None, Some("model-new"), Some("max"), "有理由")
        .expect("合法修订");
    let rendered = format!("{record:?}");

    for needle in ["executor-alpha", "model-old", "model-new", "xhigh", "max", "有理由"] {
        assert!(
            rendered.contains(needle),
            "修订记录必须含 agent / before / after / reason，缺 {needle}。实际: {rendered}"
        );
    }
    assert!(
        rendered.contains(&genesis),
        "修订记录必须含 registryDigestBefore，否则折叠无法验证起点"
    );
    let current = agent_registry_digest(&root).expect("修订后摘要");
    assert!(
        rendered.contains(&current),
        "修订记录必须含 registryDigestAfter"
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_amendment_entry_fails_closed_on_illegal_states() {
    let root = temp_root("fail-closed");
    write_fixture_registry(&root);

    // 不存在的 agent
    assert!(
        amend_agent_pin(&root, "executor-nonexistent", None, Some("m"), None, "r").is_err(),
        "不在 registry 里的 agent 必须拒绝"
    );
    // 空白 reason
    assert!(
        amend_agent_pin(&root, "executor-alpha", None, Some("m"), None, "   ").is_err(),
        "空白 reason 必须拒绝——修订必须可归因"
    );
    // 三字段全空
    assert!(
        amend_agent_pin(&root, "executor-alpha", None, None, None, "r").is_err(),
        "三字段全空的修订是空转，必须拒绝"
    );
    // 同值空转
    assert!(
        amend_agent_pin(&root, "executor-alpha", None, Some("model-old"), None, "r").is_err(),
        "与现值完全相同的修订是空转，必须拒绝"
    );

    fs::remove_dir_all(root).unwrap();
}

/// M9 的载体：任何被拒绝的修订都不得留下半改的文件。
#[test]
fn a_refused_amendment_leaves_the_registry_byte_identical() {
    let root = temp_root("atomic");
    write_fixture_registry(&root);
    let before = registry_text(&root);

    for (agent, model, reason) in [
        ("executor-nonexistent", Some("m"), "r"),
        ("executor-alpha", None, "r"),
        ("executor-alpha", Some("m"), "  "),
    ] {
        let _ = amend_agent_pin(&root, agent, None, model, None, reason);
        assert_eq!(
            before,
            registry_text(&root),
            "被拒绝的修订 ({agent}) 留下了字节改动——事务不是原子的"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

// ------------------------------------------------- ⑥ 接线：读源码，不自调

/// 只读校验必须**真的**消费折叠函数。折叠函数自测通过，不等于校验路径用了它。
#[test]
fn the_readonly_validation_folds_amendments() {
    let source = read_repo("orch/crates/orch-host/src/plan.rs");
    assert!(
        source.contains("expected_registry_digest_with_amendments"),
        "plan.rs 的只读 IR 校验路径必须调用折叠函数；\
         不调用它就意味着「不需要 replan」是靠别的方式做到的——大概率是把校验放宽了"
    );
}

/// M4 的载体：修订入口绝不允许自己补签。
#[test]
fn an_amendment_never_signs_the_plan_itself() {
    let registry_src = read_repo("orch/crates/orch-host/src/registry.rs");
    assert!(
        !registry_src.contains("PlanSignedOff"),
        "修订入口不得写 PlanSignedOff。签核是 HITL#1、actor=user、\
         「永不被其他命令隐式触发」；入口自己补签等于伪造用户意志"
    );
}

/// M7 的载体：后端回执对账必须继续读 `WakeIssued` 里**记录**的 pin，
/// 而不是修订后的当前 registry。否则一次轮中修订会追溯改写在飞 attempt 的判据。
#[test]
fn backend_reconciliation_still_reads_the_recorded_wake_pin() {
    let source = read_repo("orch/crates/orch-host/src/wake.rs");
    assert!(
        source.contains("facts.requested_provider") && source.contains("facts.requested_model"),
        "回执对账必须继续以 WakeIssued 记录的 requested_* 为准（B283 建立的语义）"
    );
    assert!(
        !source.contains("load_agent_definitions") || source.contains("facts.requested_model"),
        "wake.rs 不得改用当前 registry 做回执对账"
    );
}

// ------------------------------------------------- ⑦ 管道 marker

#[test]
fn the_support_module_is_wired() {
    audited_agent_pin_amendment_support::contract_loaded();
}
