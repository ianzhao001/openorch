//! ═══ 红种子契约 · B22 ═══（落位: orch/crates/orch-host/tests/cas_cache_key.rs，逐字节复制）
//! 预期红（redForm: compile）：cas::cache_key 尚不存在（E0425，文件级编译红）。
//! 规格来源：design/06 §7——缓存键 = base_sha + head_sha + commandDigest + environmentDigest
//!   + tool_versions，任一变化即 miss，**严禁在新 HEAD 复用旧绿**。
//! 变异清单（E9 下界）: M1 键计算忽略 head_sha → ③ 红；M2 tool_versions 不做排序归一 → ② 红；M3 键计算忽略 resolved_command → ④ 红
use orch_host::cas;

fn tv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[test]
fn key_is_deterministic_64_hex() {
    // ① 同输入同键；sha256 hex 定长 64
    let k1 = cas::cache_key("b1", "h1", "cargo test", "env-a", &tv(&[("rustc", "1.97.1")]));
    let k2 = cas::cache_key("b1", "h1", "cargo test", "env-a", &tv(&[("rustc", "1.97.1")]));
    assert_eq!(k1, k2);
    assert_eq!(k1.len(), 64);
    assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn tool_versions_order_insensitive() {
    // ② tool_versions 是集合语义：序不同键必须相同（内部排序归一）
    let a = cas::cache_key("b", "h", "c", "e", &tv(&[("rustc", "1.97"), ("cargo", "1.97")]));
    let b = cas::cache_key("b", "h", "c", "e", &tv(&[("cargo", "1.97"), ("rustc", "1.97")]));
    assert_eq!(a, b);
}

#[test]
fn new_head_never_reuses_old_green() {
    // ③ head_sha 变则键变——「严禁在新 HEAD 复用旧绿」的机械形
    let old = cas::cache_key("b", "h-old", "c", "e", &tv(&[("rustc", "1.97")]));
    let new = cas::cache_key("b", "h-new", "c", "e", &tv(&[("rustc", "1.97")]));
    assert_ne!(old, new);
}

#[test]
fn command_change_invalidates() {
    // ④ resolved_command 变则键变（testFast 的绿不能顶 testFull 的账）
    let fast = cas::cache_key("b", "h", "cargo test --lib", "e", &tv(&[("rustc", "1.97")]));
    let full = cas::cache_key("b", "h", "cargo test --workspace", "e", &tv(&[("rustc", "1.97")]));
    assert_ne!(fast, full);
}
