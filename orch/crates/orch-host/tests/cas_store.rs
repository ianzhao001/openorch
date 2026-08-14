//! ═══ 红种子契约 · B18c ═══（落位: orch/crates/orch-host/tests/cas_store.rs，逐字节复制）
//! 预期红（redForm: compile）：orch_host::cas 为占位空模块。
//! 变异清单（E9 下界）: M1 hash 掺时间戳（不再内容寻址）→ ② 红；M2 get 不校验存在 → ③ 红；M3 分片目录取消 → ④ 红
use orch_host::cas::Store;

fn tmp_store(tag: &str) -> Store {
    let dir = orch_host::util::test_scratch_dir(&format!("cas-seed-{tag}"));
    Store::new(&dir)
}

#[test]
fn put_get_roundtrip() {
    // ① 往返一致；hash 为 64 位 hex
    let s = tmp_store("rt");
    let h = s.put(b"evidence-bytes").expect("put");
    assert_eq!(h.len(), 64);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(s.get(&h).expect("get"), Some(b"evidence-bytes".to_vec()));
}

#[test]
fn identical_content_same_hash() {
    // ② 内容寻址：同内容重复 put 同 hash
    let s = tmp_store("dedup");
    let h1 = s.put(b"same").unwrap();
    let h2 = s.put(b"same").unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn unknown_hash_is_none() {
    // ③ 未知 hash → None（不 panic 不 Err）
    let s = tmp_store("miss");
    let missing = "0".repeat(64);
    assert_eq!(s.get(&missing).unwrap(), None);
}

#[test]
fn objects_are_sharded_by_prefix() {
    // ④ 落盘布局：objects/<hash前2>/<hash后62>
    let s = tmp_store("shard");
    let h = s.put(b"shard-me").unwrap();
    assert!(s.object_path(&h).ends_with(format!("objects/{}/{}", &h[..2], &h[2..])));
    assert!(s.object_path(&h).is_file());
}
