//! ═══ 红种子契约 · B24 ═══（落位: orch/crates/orch-host/tests/gate_cache.rs，逐字节复制）
//! 预期红（redForm: compile）：gate::record_evidence / lookup_evidence 尚不存在（E0425，文件级编译红）。
//! 规格来源：design/06 §7（Evidence schema：artifact 落 CAS + 缓存键）——B22 的 cache_key/keyed_put
//!   已入库，本棒把门证据接上：exit_code+日志入 CAS，键→证据可回查。
//! 变异清单（E9 下界）: M1 lookup 恒 None → ① 红；M2 record 忽略 exit_code 恒存 0 → ③ 红；M3 同 key 二次 record 不覆盖（首写 wins） → ④ 红
use orch_host::{cas, gate};

fn store(tag: &str) -> cas::Store {
    let dir = orch_host::util::test_scratch_dir(&format!("b24-gate-cache-{tag}"));
    cas::Store::new(&dir)
}

#[test]
fn record_then_lookup_roundtrip() {
    // ① 记录→回查一致；日志真实入 CAS（凭 log_sha256 可取回原文）
    let s = store("roundtrip");
    let ev = gate::record_evidence(&s, "k1", 0, b"log-a").unwrap();
    assert_eq!(ev.key, "k1");
    assert_eq!(ev.exit_code, 0);
    assert_eq!(s.get(&ev.log_sha256).unwrap(), Some(b"log-a".to_vec()));
    let back = gate::lookup_evidence(&s, "k1").unwrap();
    assert_eq!(back.exit_code, 0);
    assert_eq!(back.log_sha256, ev.log_sha256);
}

#[test]
fn miss_returns_none() {
    // ② 无记录键 → None（miss 必须显式，绝不臆造旧绿）
    let s = store("miss");
    assert!(gate::lookup_evidence(&s, "absent").is_none());
}

#[test]
fn keys_are_isolated() {
    // ③ 不同键互不串账（exit_code 与日志各归各）
    let s = store("isolated");
    gate::record_evidence(&s, "k1", 0, b"log-a").unwrap();
    gate::record_evidence(&s, "k2", 1, b"log-b").unwrap();
    let e1 = gate::lookup_evidence(&s, "k1").unwrap();
    let e2 = gate::lookup_evidence(&s, "k2").unwrap();
    assert_eq!(e1.exit_code, 0);
    assert_eq!(e2.exit_code, 1);
    assert_ne!(e1.log_sha256, e2.log_sha256);
}

#[test]
fn same_key_overwrite_latest_wins() {
    // ④ 同键重录=覆盖语义（B22 卡已定：碰撞语义=同键覆盖），最新为准
    let s = store("overwrite");
    gate::record_evidence(&s, "k1", 1, b"log-old").unwrap();
    gate::record_evidence(&s, "k1", 0, b"log-new").unwrap();
    let e = gate::lookup_evidence(&s, "k1").unwrap();
    assert_eq!(e.exit_code, 0);
    assert_eq!(s.get(&e.log_sha256).unwrap(), Some(b"log-new".to_vec()));
}
