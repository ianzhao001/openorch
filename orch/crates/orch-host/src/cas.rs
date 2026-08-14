//! Evidence CAS（B18c）：内容寻址存储核心（design/06 §6 雏形）。
//! 契约：orch/crates/orch-host/tests/cas_store.rs（红种子逐字节落位）。
//! 布局：<root>/objects/<hash前2>/<hash后62>；原子写（tmp+rename）防半写；同内容幂等。

use anyhow::{Context, Result};
use sha2::Digest;
use std::fs;
use std::path::{Path, PathBuf};

/// 内容寻址存储：key = 内容 sha256 的 64 位小写 hex
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// 惰性建目录：此处只记路径，首次 put 才落盘建目录
    pub fn new(dir: &Path) -> Store {
        Store { root: dir.to_path_buf() }
    }

    /// 写入内容，返回 sha256 hex64；同内容重复 put 幂等（同 hash，不重复落盘）
    pub fn put(&self, data: &[u8]) -> Result<String> {
        let hash = hex::encode(sha2::Sha256::digest(data));
        let path = self.object_path(&hash);
        if path.is_file() {
            return Ok(hash);
        }
        let shard = path.parent().context("object_path 无父目录")?;
        fs::create_dir_all(shard).context("创建分片目录失败")?;
        // 原子写：先写同目录临时文件再 rename，崩溃不留半写对象
        let tmp = shard.join(format!(".tmp-{}-{}", std::process::id(), &hash));
        fs::write(&tmp, data).context("写临时对象失败")?;
        fs::rename(&tmp, &path).context("临时对象改名失败")?;
        Ok(hash)
    }

    /// 按 hash 读内容；未知 hash → Ok(None)（不 panic 不 Err）
    pub fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        let path = self.object_path(hash);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(fs::read(&path).context("读对象失败")?))
    }

    /// 对象落盘路径：<root>/objects/<hash前2>/<hash后62>（短 hash 兜底不切片）
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let objects = self.root.join("objects");
        if hash.len() > 2 {
            objects.join(&hash[..2]).join(&hash[2..])
        } else {
            objects.join(hash)
        }
    }

    /// 键索引目录：<root>/keys/<key前2>/<key全> 内容为对象 hash hex
    fn key_path(&self, key: &str) -> PathBuf {
        let keys = self.root.join("keys");
        if key.len() > 2 {
            keys.join(&key[..2]).join(key)
        } else {
            keys.join(key)
        }
    }
}

/// Evidence 缓存键（design/06 §7）：base_sha + head_sha + commandDigest
/// + environmentDigest + tool_versions，任一变化即 miss——严禁在新 HEAD 复用旧绿。
/// 五要素全部参与 sha256 摘要；tool_versions 集合语义（内部排序归一）；
/// 用「长度前缀+值」定界序列化防「ab+c」=「a+bc」拼接歧义。
pub fn cache_key(
    base_sha: &str,
    head_sha: &str,
    resolved_command: &str,
    environment_digest: &str,
    tool_versions: &[(String, String)],
) -> String {
    use sha2::Sha256;
    let mut hasher = Sha256::new();
    // 长度前缀定界：每段先写 8 位长度再写值，消除拼接歧义
    let mut buf = String::new();
    let framed = |out: &mut String, s: &str| {
        out.push_str(&format!("{}:", s.len()));
        out.push_str(s);
        out.push('\n');
    };
    framed(&mut buf, base_sha);
    framed(&mut buf, head_sha);
    framed(&mut buf, resolved_command);
    framed(&mut buf, environment_digest);
    // tool_versions 排序归一（集合语义）
    let mut sorted = tool_versions.to_vec();
    sorted.sort();
    for (k, v) in &sorted {
        framed(&mut buf, k);
        framed(&mut buf, v);
    }
    hasher.update(buf.as_bytes());
    hex::encode(hasher.finalize())
}

/// 键→CAS 对象的薄索引写入：put 内容得 hash，再写键→hash 映射（同键覆盖）。
pub fn keyed_put(store: &Store, key: &str, data: &[u8]) -> Result<String> {
    let hash = store.put(data)?;
    let key_path = store.key_path(key);
    let shard = key_path.parent().context("key_path 无父目录")?;
    fs::create_dir_all(shard).context("创建键索引分片目录失败")?;
    let tmp = shard.join(format!(".tmp-{}-{}", std::process::id(), key));
    fs::write(&tmp, &hash).context("写键索引临时文件失败")?;
    fs::rename(&tmp, &key_path).context("键索引改名失败")?;
    Ok(hash)
}

/// 键→CAS 对象读取：查键索引得 hash，再 get 对象内容；无键 → Ok(None)。
pub fn keyed_get(store: &Store, key: &str) -> Result<Option<Vec<u8>>> {
    let key_path = store.key_path(key);
    if !key_path.is_file() {
        return Ok(None);
    }
    let hash = fs::read_to_string(&key_path).context("读键索引失败")?;
    store.get(hash.trim())
}

#[cfg(test)]
mod tests {
    use super::Store;

    fn tmp_store(tag: &str) -> (std::path::PathBuf, Store) {
        // store 落在 scratch 子目录：保留「new 不落盘、首 put 才建目录」的惰性语义
        let dir = crate::util::test_scratch_dir(&format!("cas-own-{tag}")).join("store");
        (dir.clone(), Store::new(&dir))
    }

    #[test]
    fn binary_and_empty_roundtrip_leaves_no_tmp_residue() {
        // 空内容与非 UTF-8 二进制往返一致；原子写不留 .tmp- 残骸
        let (_dir, s) = tmp_store("bin");
        for data in [b"".as_slice(), &[0x00, 0xFF, 0x80, 0x7F][..]] {
            let h = s.put(data).expect("put");
            assert_eq!(s.get(&h).expect("get"), Some(data.to_vec()));
        }
        let mut stack = vec![_dir.join("objects")];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let p = e.path();
                    assert!(
                        !e.file_name().to_string_lossy().starts_with(".tmp-"),
                        "临时文件残骸：{p:?}"
                    );
                    if p.is_dir() {
                        stack.push(p);
                    }
                }
            }
        }
    }

    #[test]
    fn dir_created_lazily_on_first_put() {
        // 惰性：new 不落盘，首次 put 才建目录
        let (dir, s) = tmp_store("lazy");
        assert!(!dir.exists(), "new 不应落盘建目录");
        s.put(b"wake").expect("put");
        assert!(dir.join("objects").is_dir(), "put 后 objects 目录应存在");
    }

    #[test]
    fn keyed_put_get_roundtrip() {
        use super::{cache_key, keyed_get, keyed_put};
        let (_dir, s) = tmp_store("keyed");
        let key = cache_key("b", "h", "cargo test", "env-a", &[("rustc".into(), "1.97".into())]);
        let h = keyed_put(&s, &key, b"green-evidence").expect("keyed_put");
        assert_eq!(h.len(), 64);
        let got = keyed_get(&s, &key).expect("keyed_get");
        assert_eq!(got, Some(b"green-evidence".to_vec()));
        // 同键覆盖
        keyed_put(&s, &key, b"new-evidence").expect("overwrite");
        let got2 = keyed_get(&s, &key).expect("keyed_get 2");
        assert_eq!(got2, Some(b"new-evidence".to_vec()));
    }

    #[test]
    fn cache_key_concat_ambiguity_guard() {
        use super::cache_key;
        // 拼接歧义防护：base="a"+head="bc" 不得 == base="ab"+head="c"
        let k1 = cache_key("a", "bc", "c", "e", &[]);
        let k2 = cache_key("ab", "c", "c", "e", &[]);
        assert_ne!(k1, k2, "长度前缀定界失效：拼接歧义未被防住");
    }
}
