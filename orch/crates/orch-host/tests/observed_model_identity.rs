//! B235 seeded-red contract: 声明 / 自报 / 观测三层勾稽——orch 请求的模型是否真的被供给
//! （用户 2026-08-05 需求 2 的兑现面；fusion 定稿 D6/D9）。
//!
//! Expected red: compile。`orch_host::observed::*` 尚不存在（E0432，文件级编译红）。
//!
//! ── 为什么必须有观测层（r65 轮前探针实测，research/r65-codex-pi-model-probe.md）──
//! 向 pi 请求一个**不存在**的模型 `deepseek-chat`，它只打印一行警告
//! `Model "deepseek-chat" not found for provider "deepseek". Using custom model id.`
//! 就继续跑完，exit 0；帧里 `"model":"deepseek-chat"` 是**请求回显**，
//! 而 `"responseModel":"deepseek-v4-flash"` 才是服务端真正供给的模型。
//! 也就是说：**「请求了什么」与「供给了什么」可以完全无感地分叉**，
//! 只看声明或只看自报都发现不了。三层各自入账、互不冒充，是这张卡的全部要点。
//!
//! ── 为什么 strict 比对必须带 provider（fusion D6，codex 提出、planner 复核）──
//! 既有 `probe::normalize_model_id`（probe.rs:47-54）会小写并**丢掉 provider 前缀**，
//! 于是 `deepseek/deepseek-v4-flash` 与 `opencode/deepseek-v4-flash` 会被判成同一个模型
//! （实测这两者确实是不同供给：前者 1M 上下文，后者 200K，r62/B204 曾撞 stopReason:length）。
//! normalize 只保留给「自报文本」这一层做兼容，strict 层一律按 `{provider, id}` 比。
//!
//! ── 本种子刻意不做的事 ──
//! 不用 struct literal 构造 `ModelIdentity`（一律走 `parse`），避免重演 B34/B93 的冻结字段陷阱
//! （fusion D1，见 B233 种子头）。SQLite 侧不引入新依赖：观测走 `sqlite3` CLI 只读 URI，
//! 既避开新 crate，也避免把 opencode 的私有表结构当成稳定公共 API 来编译期绑定。
//!
//! Negative mutations that must turn the named case red:
//! M1. pi 观测取帧内 `model`（请求回显）而不是 `responseModel`
//!     -> `pi_response_model_is_the_authority_not_the_request_echo` 红。
//! M2. 没有 responseModel 时伪造一个「相符」
//!     -> `pi_frames_without_response_model_are_missing_not_matched` 红。
//! M3. 观测与声明不符却判通过
//!     -> `strict_mismatch_is_reported_as_mismatch` 红。
//! M4. 别名表失效（同源不同 local id 的合法等价被误判为失配）
//!     -> `declared_alias_accepts_the_observed_identity` 红。
//! M5. 证据缺失被当成失配（advisory 被误升 strict，工具没日志就永远过不了门）
//!     -> `missing_evidence_is_not_a_mismatch` 红。
//! M6. 读取失败被当成相符（最危险的一种：出错就放行）
//!     -> `unreadable_source_never_masquerades_as_match` 红。
//! M7. 比对丢掉 provider（跨 provider 同名模型被合并）
//!     -> `provider_qualified_comparison_does_not_merge_same_id_across_providers` 红。
//! M8. opencode 观测不按 session/时间窗过滤（1.5 GB 库全表扫，且会读到别的实例的消息）
//!     -> `opencode_db_lookup_is_session_and_window_scoped` 红。

use std::fs;
use std::process::Command;

use orch_host::observed::{
    extract_pi_response_model, read_opencode_identity, reconcile_identity, ModelIdentity,
    ObservedEvidence, ObservedOutcome,
};
use orch_host::util::test_scratch_dir;

/// pi `--mode json` 的真实帧形摘录（2026-08-05 实测，请求 deepseek-chat、实供 deepseek-v4-flash）。
const PI_FRAMES: &str = r#"{"type":"message_start","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat","stopReason":"pending"}}
{"type":"message_end","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat","stopReason":"stop","responseModel":"deepseek-v4-flash"}}
{"type":"turn_end","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat","stopReason":"stop","responseModel":"deepseek-v4-flash"}}
{"type":"agent_settled"}
"#;

const PI_FRAMES_NO_RESPONSE_MODEL: &str = r#"{"type":"message_start","message":{"role":"assistant","provider":"deepseek","model":"deepseek-chat"}}
{"type":"agent_settled"}
"#;

fn sqlite3_available() -> bool {
    Command::new("sqlite3")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[test]
fn pi_response_model_is_the_authority_not_the_request_echo() {
    // M1：帧里两个字段都叫「模型」，只有一个是事实。
    let evidence = extract_pi_response_model(PI_FRAMES);
    let ObservedEvidence::Found(identity) = evidence else {
        panic!("含 responseModel 的帧流必须产出观测证据，实得 {evidence:?}");
    };
    assert_eq!(
        identity.id, "deepseek-v4-flash",
        "观测值必须取 responseModel（服务端真实供给）"
    );
    assert_ne!(
        identity.id, "deepseek-chat",
        "帧内 model 只是请求回显，不得当作观测"
    );
}

#[test]
fn pi_frames_without_response_model_are_missing_not_matched() {
    // M2：pi 声明 responseModel 必有；真缺了就是证据缺失，不能就地编一个。
    let evidence = extract_pi_response_model(PI_FRAMES_NO_RESPONSE_MODEL);
    assert!(
        matches!(evidence, ObservedEvidence::Missing),
        "无 responseModel 时必须报证据缺失，实得 {evidence:?}"
    );
}

#[test]
fn strict_mismatch_is_reported_as_mismatch() {
    // M3：这正是探针里那一幕——请求 deepseek-chat、实供 deepseek-v4-flash，全程 exit 0。
    let declared = ModelIdentity::parse("deepseek/deepseek-chat");
    let observed = ModelIdentity::parse("deepseek/deepseek-v4-flash");
    let outcome = reconcile_identity(&declared, &[], &ObservedEvidence::Found(observed));
    assert!(
        matches!(outcome, ObservedOutcome::Mismatch { .. }),
        "声明与观测不符必须判失配，实得 {outcome:?}"
    );
}

#[test]
fn declared_alias_accepts_the_observed_identity() {
    // M4：zcode 的本地 id 与上游 id 不同名（`dcc-dewu-ep/gpt-5-dcc-glm-5-2` ↔ `dewu-ep/glm-5.2`），
    // 这类合法等价靠每 agent 显式声明的别名表承认，而不是靠丢字段的模糊匹配。
    let declared = ModelIdentity::parse("dcc-dewu-ep/gpt-5-dcc-glm-5-2");
    let accepts = vec![ModelIdentity::parse("dewu-ep/glm-5.2")];
    let observed = ModelIdentity::parse("dewu-ep/glm-5.2");
    let outcome = reconcile_identity(&declared, &accepts, &ObservedEvidence::Found(observed));
    assert!(
        matches!(outcome, ObservedOutcome::Match),
        "显式声明的等价别名必须被接受，实得 {outcome:?}"
    );
}

#[test]
fn missing_evidence_is_not_a_mismatch() {
    // M5：证据不存在 ≠ 证据不符。codex/agy 目前没有稳定的服务端观测源，
    // 若把「没日志」判成失配，这些席位会永远过不了门。
    let declared = ModelIdentity::parse("openai/gpt-5.6-sol");
    let outcome = reconcile_identity(&declared, &[], &ObservedEvidence::Missing);
    assert!(
        matches!(outcome, ObservedOutcome::Missing),
        "证据缺失必须单独成态，不得判失配，实得 {outcome:?}"
    );
    assert!(
        !matches!(outcome, ObservedOutcome::Match),
        "证据缺失更不得判相符"
    );
}

#[test]
fn unreadable_source_never_masquerades_as_match() {
    // M6：读不动（库被锁、路径不存在、格式变了）只能如实说读不动。
    // 「出错就放行」是这条链上最危险的一种退化。
    let declared = ModelIdentity::parse("dewu-ep/glm-5.2");
    let outcome = reconcile_identity(
        &declared,
        &[],
        &ObservedEvidence::Unreadable("database is locked".to_string()),
    );
    assert!(
        matches!(outcome, ObservedOutcome::Unreadable { .. }),
        "读取失败必须如实上报，实得 {outcome:?}"
    );
    assert!(!matches!(outcome, ObservedOutcome::Match));
}

#[test]
fn provider_qualified_comparison_does_not_merge_same_id_across_providers() {
    // M7：`deepseek/deepseek-v4-flash`（1M 上下文）与 `opencode/deepseek-v4-flash-free`
    // 是不同供给；即便 id 段完全同名，跨 provider 也必须判失配。
    let declared = ModelIdentity::parse("deepseek/deepseek-v4-flash");
    let observed = ModelIdentity::parse("opencode/deepseek-v4-flash");
    assert_eq!(declared.id, observed.id, "前提：两者 id 段同名");
    assert_ne!(declared.provider, observed.provider, "前提：provider 不同");

    let outcome = reconcile_identity(&declared, &[], &ObservedEvidence::Found(observed));
    assert!(
        matches!(outcome, ObservedOutcome::Mismatch { .. }),
        "跨 provider 的同名模型不得被合并为相符，实得 {outcome:?}"
    );
}

#[test]
fn opencode_db_lookup_is_session_and_window_scoped() {
    // M8：opencode 的流式 JSON 不含模型字段，权威源是本机 sqlite 库的 assistant 消息
    // （实测 message.data 含 modelID/providerID/variant）。库有 1.5 GB 且多实例共用，
    // 因此查询必须同时按 session 与时间窗收敛，否则既慢又会读到另一个实例的消息。
    if !sqlite3_available() {
        eprintln!("[b235] 跳过：本机无 sqlite3 CLI（观测层按设计经该 CLI 只读访问）");
        return;
    }
    let root = test_scratch_dir("b235-opencode-db");
    let db = root.join("opencode.db");
    let sql = r#"CREATE TABLE message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);
INSERT INTO message VALUES ('m1','ses_target',1785917295500,1785917295600,
  '{"role":"assistant","modelID":"glm-5.2","providerID":"dewu-ep","variant":"high"}');
INSERT INTO message VALUES ('m2','ses_other',1785917295550,1785917295650,
  '{"role":"assistant","modelID":"deepseek-v4-flash-free","providerID":"opencode","variant":"minimal"}');
INSERT INTO message VALUES ('m3','ses_target',1785900000000,1785900000100,
  '{"role":"assistant","modelID":"stale-model","providerID":"stale-provider","variant":"high"}');
"#;
    let status = Command::new("sqlite3")
        .arg(&db)
        .arg(sql)
        .status()
        .expect("sqlite3 建夹具失败");
    assert!(status.success(), "sqlite3 建夹具应成功");

    // 只读访问的机械佐证之一：把库文件设为只读后仍必须读得出来。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&db, fs::Permissions::from_mode(0o444)).expect("设只读权限失败");
    }

    let hit = read_opencode_identity(&db, "ses_target", 1785917295000, 1785917296000);
    let ObservedEvidence::Found(identity) = hit else {
        panic!("窗口内应查到目标 session 的 assistant 消息，实得 {hit:?}");
    };
    assert_eq!(identity.provider.as_deref(), Some("dewu-ep"));
    assert_eq!(identity.id, "glm-5.2");

    let other_session = read_opencode_identity(&db, "ses_absent", 1785917295000, 1785917296000);
    assert!(
        matches!(other_session, ObservedEvidence::Missing),
        "别的 session 的消息不得被算作本次观测，实得 {other_session:?}"
    );

    let out_of_window = read_opencode_identity(&db, "ses_target", 1785917295000, 1785917295400);
    assert!(
        matches!(out_of_window, ObservedEvidence::Missing),
        "窗口外的历史消息不得被算作本次观测，实得 {out_of_window:?}"
    );

    assert!(
        !db.with_extension("db-wal").exists(),
        "只读访问不得产生 WAL 副产物"
    );
}
